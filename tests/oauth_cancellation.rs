//! Browser sign-in completion races: cancellation during the token exchange and during the
//! account-lock wait, expiry during the exchange, duplicate callbacks, provider failure, and a
//! randomized cancel-versus-complete stress run. A flow ends exactly once; a cancelled or expired
//! flow never saves an account; the callback listener is released. Loopback mock token endpoint
//! and fake identities only.

use axum::{Router, body::Bytes, extract::State, http::StatusCode, routing::post};
use base64::Engine;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use switchyard::{
    app::{App, AppState, account_lock},
    oauth,
    store::Connection,
};

/// OAuth endpoint and TTL overrides are process-global: these tests run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn app() -> (tempfile::TempDir, App) {
    let d = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let app = AppState::new(d.path().to_path_buf(), "127.0.0.1".into(), 1, 8, 30).unwrap();
    (d, app)
}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn jwt(payload: Value) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(payload.to_string())
    )
}

/// What the mock token endpoint does with the next exchange.
#[derive(Clone)]
struct Reply {
    delay: Duration,
    status: u16,
    /// Identity (`sub`) of the returned tokens; distinct subs are distinct accounts.
    sub: String,
    tag: String,
}
#[derive(Clone)]
struct Mock {
    reply: Arc<Mutex<Reply>>,
    hits: Arc<AtomicUsize>,
}
impl Mock {
    async fn start() -> (Self, String) {
        let m = Mock {
            reply: Arc::new(Mutex::new(Reply {
                delay: Duration::ZERO,
                status: 200,
                sub: "auth0|one".into(),
                tag: "t1".into(),
            })),
            hits: Arc::new(AtomicUsize::new(0)),
        };
        let router = Router::new()
            .route(
                "/token",
                post(|State(m): State<Mock>, _b: Bytes| async move {
                    m.hits.fetch_add(1, Ordering::SeqCst);
                    let r = m.reply.lock().unwrap().clone();
                    tokio::time::sleep(r.delay).await;
                    let auth = json!({"chatgpt_account_id":"ws-1","chatgpt_plan_type":"plus"});
                    let exp = chrono::Utc::now().timestamp() + 86400;
                    let body = json!({
                        "access_token": jwt(json!({"exp":exp,"sub":r.sub,"tag":r.tag,"https://api.openai.com/auth":auth})),
                        "refresh_token": format!("rt-{}", r.tag),
                        "id_token": jwt(json!({"sub":r.sub,"email":"m@example.com","https://api.openai.com/auth":auth})),
                    });
                    (StatusCode::from_u16(r.status).unwrap(), axum::Json(body))
                }),
            )
            .with_state(m.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", l.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(l, router).await;
        });
        (m, url)
    }
    fn set(&self, delay_ms: u64, status: u16, sub: &str, tag: &str) {
        *self.reply.lock().unwrap() = Reply {
            delay: Duration::from_millis(delay_ms),
            status,
            sub: sub.into(),
            tag: tag.into(),
        };
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
    async fn wait_for_hits(&self, n: usize) {
        for _ in 0..250 {
            if self.hits() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
        panic!("token endpoint never reached {n} requests");
    }
}

struct Flow {
    id: String,
    state: String,
    port: u16,
}
async fn begin(app: &App, port: u16, token_url: &str) -> Flow {
    oauth::set_test_endpoints("codex", port, token_url, None);
    let v = oauth::start(app.clone(), "codex").await.unwrap();
    let url = url::Url::parse(v["authorization_url"].as_str().unwrap()).unwrap();
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    Flow {
        id: v["id"].as_str().unwrap().into(),
        state,
        port,
    }
}
/// The browser redirect to the loopback listener. Returns the page status, or `None` if the
/// listener is gone.
async fn browser_callback(port: u16, state: &str) -> Option<u16> {
    let url = format!("http://127.0.0.1:{port}/auth/callback?code=the-code&state={state}");
    reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()
        .map(|r| r.status().as_u16())
}
fn status(app: &App, id: &str) -> Value {
    oauth::status(app, id).unwrap()
}
fn connections(app: &App) -> Vec<Connection> {
    app.store.list("connection")
}
async fn listener_released(port: u16) -> bool {
    for _ in 0..100 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn cancelling_during_the_token_exchange_saves_nothing_and_frees_the_port() {
    let _s = SERIAL.lock().await;
    oauth::set_test_ttl(None);
    let (m, url) = Mock::start().await;
    let (_d, app) = app();
    m.set(3000, 200, "auth0|one", "late");
    let f = begin(&app, free_port(), &url).await;
    let port = f.port;
    let state = f.state.clone();
    let callback = tokio::spawn(async move { browser_callback(port, &state).await });
    m.wait_for_hits(1).await;
    assert_eq!(
        status(&app, &f.id)["status"],
        "pending",
        "exchanging reads as pending"
    );

    let started = Instant::now();
    let v = oauth::cancel(&app, &f.id).unwrap();
    assert_eq!(v["status"], "error");
    assert!(v["message"].as_str().unwrap().contains("cancelled"), "{v}");
    // The in-flight callback is abandoned promptly, not after the provider's 3 s.
    assert_eq!(callback.await.unwrap(), Some(409));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        listener_released(f.port).await,
        "callback listener released"
    );

    // Even after the provider would have answered, nothing is saved and the outcome holds.
    tokio::time::sleep(Duration::from_millis(3200)).await;
    assert!(connections(&app).is_empty());
    assert_eq!(status(&app, &f.id)["status"], "error");
    // A late paste of the callback reports the final outcome, not a state mismatch.
    let e = oauth::submit_callback(&app, &f.id, "the-code#wrong-state")
        .await
        .unwrap_err();
    assert_eq!(e.status.as_u16(), 409);
    assert!(e.message.contains("cancelled"), "{}", e.message);
}

#[tokio::test]
async fn cancelling_while_the_account_lock_is_held_saves_nothing() {
    let _s = SERIAL.lock().await;
    oauth::set_test_ttl(None);
    let (m, url) = Mock::start().await;
    let (_d, app) = app();

    // First sign-in creates the account.
    m.set(0, 200, "auth0|one", "first");
    let f = begin(&app, free_port(), &url).await;
    assert_eq!(browser_callback(f.port, &f.state).await, Some(200));
    let saved = connections(&app);
    assert_eq!(saved.len(), 1);
    let original = saved[0].api_key.clone();

    // Second sign-in for the same account must wait for the account lock (held by "a refresh").
    let lock = account_lock(&app, &saved[0].id);
    let held = lock.lock().await;
    m.set(0, 200, "auth0|one", "second");
    let f = begin(&app, free_port(), &url).await;
    let id = f.id.clone();
    let port = f.port;
    let state = f.state.clone();
    let callback = tokio::spawn(async move { browser_callback(port, &state).await });
    m.wait_for_hits(2).await;
    tokio::time::sleep(Duration::from_millis(50)).await; // now waiting on the account lock
    assert_eq!(oauth::cancel(&app, &id).unwrap()["status"], "error");
    assert_eq!(callback.await.unwrap(), Some(409), "lock wait abandoned");
    drop(held);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = connections(&app);
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].api_key, original,
        "cancelled sign-in did not overwrite tokens"
    );
    assert_eq!(status(&app, &id)["status"], "error");
    assert!(listener_released(f.port).await);
}

#[tokio::test]
async fn expiry_during_the_exchange_cannot_be_overwritten_by_completion() {
    let _s = SERIAL.lock().await;
    let (m, url) = Mock::start().await;
    let (_d, app) = app();
    oauth::set_test_ttl(Some(Duration::from_millis(400)));
    m.set(1500, 200, "auth0|one", "slow");
    let f = begin(&app, free_port(), &url).await;
    let started = Instant::now();
    let page = browser_callback(f.port, &f.state).await;
    oauth::set_test_ttl(None);
    assert_eq!(page, Some(410), "expired while exchanging");
    assert!(started.elapsed() < Duration::from_millis(1200));
    assert_eq!(status(&app, &f.id)["status"], "expired");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(status(&app, &f.id)["status"], "expired");
    assert!(connections(&app).is_empty());
    assert!(listener_released(f.port).await);
    // Cancelling an expired sign-in changes nothing.
    assert_eq!(oauth::cancel(&app, &f.id).unwrap()["status"], "expired");
}

#[tokio::test]
async fn duplicate_callbacks_exchange_once_and_completion_survives_cancel() {
    let _s = SERIAL.lock().await;
    oauth::set_test_ttl(None);
    let (m, url) = Mock::start().await;
    let (_d, app) = app();
    m.set(300, 200, "auth0|dup", "dup");
    let f = begin(&app, free_port(), &url).await;
    let (a, b) = tokio::join!(browser_callback(f.port, &f.state), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        oauth::submit_callback(&app, &f.id, &format!("the-code#{}", f.state)).await
    });
    assert_eq!(a, Some(200));
    let e = b.unwrap_err();
    assert_eq!(e.status.as_u16(), 409, "{}", e.message);
    assert_eq!(m.hits(), 1, "one token exchange");
    assert_eq!(status(&app, &f.id)["status"], "complete");
    assert_eq!(connections(&app).len(), 1);

    // After completion: a pasted duplicate reports success; cancel is a no-op.
    let v = oauth::submit_callback(&app, &f.id, "the-code#anything")
        .await
        .unwrap();
    assert_eq!(v["status"], "complete");
    let v = oauth::cancel(&app, &f.id).unwrap();
    assert_eq!(v["status"], "complete");
    assert!(v["connection"]["id"].is_string());
    assert_eq!(connections(&app).len(), 1);
    assert_eq!(m.hits(), 1);
}

#[tokio::test]
async fn provider_failure_ends_the_flow_once() {
    let _s = SERIAL.lock().await;
    oauth::set_test_ttl(None);
    let (m, url) = Mock::start().await;
    let (_d, app) = app();
    m.set(0, 500, "auth0|x", "x");
    let f = begin(&app, free_port(), &url).await;
    assert_eq!(browser_callback(f.port, &f.state).await, Some(502));
    let v = status(&app, &f.id);
    assert_eq!(v["status"], "error");
    let message = v["message"].as_str().unwrap().to_string();
    assert!(listener_released(f.port).await);
    // Neither cancel nor a later callback rewrites the outcome.
    assert_eq!(
        oauth::cancel(&app, &f.id).unwrap()["message"],
        message.as_str()
    );
    let e = oauth::submit_callback(&app, &f.id, &format!("c#{}", f.state))
        .await
        .unwrap_err();
    assert_eq!(
        (e.status.as_u16(), e.message.as_str()),
        (409, message.as_str())
    );
    assert!(connections(&app).is_empty());
    assert_eq!(m.hits(), 1);
}

/// Cancel and completion race with random timing. Linearizability: every flow ends exactly once,
/// and an account exists for a flow if and only if that flow reports `complete`.
#[tokio::test]
async fn cancel_and_completion_races_are_linearizable() {
    let _s = SERIAL.lock().await;
    oauth::set_test_ttl(None);
    let (m, url) = Mock::start().await;
    let (_d, app) = app();
    let mut completed = Vec::new();
    for i in 0..24u64 {
        let sub = format!("auth0|race-{i}");
        m.set(i % 6 * 5, 200, &sub, &format!("race-{i}"));
        let f = begin(&app, free_port(), &url).await;
        let id = f.id.clone();
        let cancel_after = Duration::from_millis((i * 7) % 40);
        let app2 = app.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(cancel_after).await;
            oauth::cancel(&app2, &id).unwrap()
        });
        let page = browser_callback(f.port, &f.state).await;
        let cancelled = canceller.await.unwrap();
        let fin = status(&app, &f.id);
        let saved = connections(&app)
            .into_iter()
            .any(|c| c.refresh_token == format!("rt-race-{i}"));
        match fin["status"].as_str().unwrap() {
            "complete" => {
                assert!(saved, "complete flow {i} has its account");
                assert_eq!(
                    cancelled["status"], "complete",
                    "cancel after commit is a no-op"
                );
                assert!(matches!(page, Some(200) | None), "{page:?}");
                completed.push(i);
            }
            "error" => {
                assert!(!saved, "cancelled flow {i} saved an account");
                assert!(fin["message"].as_str().unwrap().contains("cancelled"));
                assert!(matches!(page, Some(409) | None), "{page:?}");
            }
            other => panic!("flow {i} ended as {other}"),
        }
        assert!(listener_released(f.port).await, "flow {i} listener");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(connections(&app).len(), completed.len());
    assert!(
        !completed.is_empty() && completed.len() < 24,
        "the run must exercise both outcomes: {completed:?}"
    );
}
