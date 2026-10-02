//! Antigravity accounts end to end against mock Google OAuth and Cloud Code servers: browser
//! sign-in (PKCE, offline access, onboarding), gateway-owned refresh, read-only `agy` and
//! CLIProxyAPI imports that never rotate source tokens, project discovery and the quota helper.
//! The OAuth client and endpoints come from test hooks; no real Google endpoint is contacted.

mod support;

use axum::{
    Router,
    extract::Form,
    http::HeaderMap,
    routing::{get, post},
};
use base64::Engine;
use reqwest::Method;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use support::*;
use switchyard::{antigravity, credentials, oauth, store::Connection};

/// The OAuth hooks are process-wide; tests in this file run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CLIENT_ID: &str = "4242-testclient.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "test-client-secret-not-real";

fn jwt(payload: Value) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(payload.to_string())
    )
}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}
fn stored(gw: &Gateway, id: &str) -> Connection {
    gw.app.store.get("connection", id).unwrap()
}

/// Mock Google OAuth + userinfo + Cloud Code control plane.
#[derive(Default)]
struct Google {
    token_forms: Mutex<Vec<HashMap<String, String>>>,
    onboard_calls: AtomicUsize,
    /// Number of onboardUser polls answered `done:false` before the project is returned.
    onboard_pending: usize,
    /// loadCodeAssist returns this project directly when set.
    direct_project: Option<String>,
    /// Access token the control plane accepts.
    accepted: Mutex<String>,
}
fn google(state: Arc<Google>) -> Router {
    let s1 = state.clone();
    let s2 = state.clone();
    let s3 = state.clone();
    let s4 = state.clone();
    let s5 = state.clone();
    let authorized = |s: &Google, h: &HeaderMap| {
        h.get("authorization").and_then(|v| v.to_str().ok())
            == Some(&format!("Bearer {}", s.accepted.lock().unwrap()))
    };
    Router::new()
        .route(
            "/token",
            post(move |Form(form): Form<HashMap<String, String>>| {
                let s = s1.clone();
                async move {
                    s.token_forms.lock().unwrap().push(form.clone());
                    let n = s.token_forms.lock().unwrap().len();
                    let access = format!("ya29.access-{n}");
                    *s.accepted.lock().unwrap() = access.clone();
                    if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
                        json_response(200, json!({"access_token":access,"expires_in":3599,"token_type":"Bearer"}))
                    } else {
                        json_response(200, json!({"access_token":access,"refresh_token":"1//refresh-secret","expires_in":3599,"token_type":"Bearer"}))
                    }
                }
            }),
        )
        .route(
            "/userinfo",
            get(move |h: HeaderMap| {
                let s = s2.clone();
                async move {
                    if !authorized(&s, &h) {
                        return json_response(401, json!({}));
                    }
                    json_response(200, json!({"id":"1093","email":"Person@Example.com","verified_email":true}))
                }
            }),
        )
        .route(
            "/v1internal:loadCodeAssist",
            post(move |h: HeaderMap| {
                let s = s3.clone();
                async move {
                    if !authorized(&s, &h) || !h.get("user-agent").and_then(|v| v.to_str().ok()).is_some_and(|u| u.starts_with("antigravity/hub/")) {
                        return json_response(401, json!({}));
                    }
                    match &s.direct_project {
                        Some(p) => json_response(200, json!({"cloudaicompanionProject":p,"currentTier":{"id":"standard-tier"}})),
                        None => json_response(200, json!({"allowedTiers":[{"id":"legacy-tier"},{"id":"free-tier","isDefault":true}]})),
                    }
                }
            }),
        )
        .route(
            "/v1internal:onboardUser",
            post(move |axum::Json(v): axum::Json<Value>| {
                let s = s4.clone();
                async move {
                    assert_eq!(v["tier_id"], "free-tier", "the default allowed tier is used");
                    let n = s.onboard_calls.fetch_add(1, Ordering::SeqCst);
                    if n < s.onboard_pending {
                        json_response(200, json!({"done":false}))
                    } else {
                        json_response(200, json!({"done":true,"response":{"cloudaicompanionProject":{"id":"onboarded-proj"}}}))
                    }
                }
            }),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            post(move |h: HeaderMap, axum::Json(v): axum::Json<Value>| {
                let s = s5.clone();
                async move {
                    if !authorized(&s, &h) {
                        return json_response(401, json!({"error":{"message":format!("bad token {PROVIDER_KEY}")}}));
                    }
                    let project = v["project"].as_str().unwrap_or("");
                    json_response(200, json!({"models":{
                        "gemini-3-flash":{"displayName":"Gemini 3 Flash","quotaInfo":{"remainingFraction":0.4,"resetTime":"2026-10-03T12:00:00Z"}},
                        "claude-sonnet-4-6":{"displayName":"Claude Sonnet 4.6","quotaInfo":{"remainingFraction":0.9}},
                        "chat_20706":{"displayName":"internal"},
                        "project-echo":{"displayName":project}
                    }}))
                }
            }),
        )
        .route("/v1internal:retrieveUserQuota", post(|| async { json_response(200, json!({"buckets":[{"modelId":"gemini-3-flash","remainingFraction":0.2}]})) }))
}

struct Env {
    _dir: tempfile::TempDir,
    gw: Gateway,
    google: Upstream,
    state: Arc<Google>,
    port: u16,
}
async fn env(state: Google) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    let state = Arc::new(state);
    let google = Upstream::start(google(state.clone())).await;
    let port = free_port();
    antigravity::set_test_client(Some((CLIENT_ID, CLIENT_SECRET)));
    antigravity::set_test_base(Some(&google.base()));
    oauth::set_test_endpoints(
        "antigravity",
        port,
        &format!("{}/token", google.base()),
        None,
    );
    oauth::set_test_ttl(None);
    Env {
        _dir: dir,
        gw,
        google,
        state,
        port,
    }
}

async fn sign_in(e: &Env) -> Value {
    let started: Value =
        e.gw.admin_send(
            Method::POST,
            "/api/oauth/start",
            json!({"provider":"antigravity"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let q = query(started["authorization_url"].as_str().unwrap());
    let callback = format!(
        "http://localhost:{}/oauth-callback?code=4%2Fgoogle-code&state={}&scope=x",
        e.port, q["state"]
    );
    let r =
        e.gw.admin_send(
            Method::POST,
            &format!("/api/oauth/{}/callback", started["id"].as_str().unwrap()),
            json!({"input":callback}),
        )
        .await;
    let text = r.text().await.unwrap();
    assert!(
        !text.contains("ya29.") && !text.contains("refresh-secret"),
        "tokens leaked: {text}"
    );
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn browser_sign_in_uses_google_pkce_offline_access_and_onboards_a_project() {
    let _serial = SERIAL.lock().await;
    let e = env(Google {
        onboard_pending: 1,
        ..Default::default()
    })
    .await;
    let r =
        e.gw.admin_send(
            Method::POST,
            "/api/oauth/start",
            json!({"provider":"antigravity"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let started: Value = r.json().await.unwrap();
    let url = started["authorization_url"].as_str().unwrap();
    assert!(
        url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"),
        "{url}"
    );
    let q = query(url);
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(
        q["redirect_uri"],
        format!("http://localhost:{}/oauth-callback", e.port)
    );
    assert_eq!(
        (q["access_type"].as_str(), q["prompt"].as_str()),
        ("offline", "consent")
    );
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].contains("https://www.googleapis.com/auth/cloud-platform"));
    assert!(
        !url.contains(CLIENT_SECRET),
        "the client secret never goes to the browser"
    );
    let id = started["id"].as_str().unwrap();
    let callback = format!(
        "http://localhost:{}/oauth-callback?code=4%2Fgoogle-code&state={}",
        e.port, q["state"]
    );
    let done: Value =
        e.gw.admin_send(
            Method::POST,
            &format!("/api/oauth/{id}/callback"),
            json!({"input":callback}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(done["status"], "complete", "{done}");
    let c = &done["connection"];
    assert_eq!(
        (c["kind"].as_str(), c["credential_source"].as_str()),
        (Some("antigravity"), Some("oauth"))
    );
    assert_eq!(c["name"], "Person@Example.com");
    assert_eq!(c["base_url"], antigravity::DAILY_BASE);
    assert_eq!(
        c["models"],
        json!(["claude-sonnet-4-6", "gemini-3-flash", "project-echo"]),
        "seeded from the live catalog, internal ids skipped"
    );

    let form = e.state.token_forms.lock().unwrap()[0].clone();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "4/google-code");
    assert_eq!(
        (form["client_id"].as_str(), form["client_secret"].as_str()),
        (CLIENT_ID, CLIENT_SECRET)
    );
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        <sha2::Sha256 as sha2::Digest>::digest(form["code_verifier"].as_bytes()),
    );
    assert_eq!(challenge, q["code_challenge"]);
    assert_eq!(
        e.state.onboard_calls.load(Ordering::SeqCst),
        2,
        "onboarding polled until done"
    );
    let s = stored(&e.gw, c["id"].as_str().unwrap());
    assert_eq!(
        s.account_id, "onboarded-proj",
        "project stored for requests"
    );
    assert_eq!(s.refresh_token, "1//refresh-secret");
    assert_eq!(
        s.account_identity,
        antigravity::identity_for_email("person@example.com")
    );
    let listed =
        e.gw.admin_get("/api/connections")
            .await
            .text()
            .await
            .unwrap();
    assert!(
        !listed.contains("ya29.")
            && !listed.contains("refresh-secret")
            && !listed.contains("onboarded-proj")
    );
}

#[tokio::test]
async fn sign_in_without_an_oauth_client_is_refused_clearly() {
    let _serial = SERIAL.lock().await;
    let e = env(Google::default()).await;
    antigravity::set_test_client(None);
    // Without the env vars or an installed app there is no client (this host has neither).
    if antigravity::oauth_client().is_some() {
        return;
    }
    let r =
        e.gw.admin_send(
            Method::POST,
            "/api/oauth/start",
            json!({"provider":"antigravity"}),
        )
        .await;
    assert_eq!(r.status(), 424);
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("SWITCHYARD_ANTIGRAVITY_CLIENT_ID")
    );
    assert!(e.state.token_forms.lock().unwrap().is_empty());
    // The callback port was never bound.
    assert!(std::net::TcpListener::bind(("127.0.0.1", e.port)).is_ok());
}

#[tokio::test]
async fn gateway_owned_sign_in_refreshes_with_the_client_and_keeps_the_refresh_token() {
    let _serial = SERIAL.lock().await;
    let e = env(Google {
        direct_project: Some("direct-proj".into()),
        ..Default::default()
    })
    .await;
    let done = sign_in(&e).await;
    let id = done["connection"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        stored(&e.gw, &id).account_id,
        "direct-proj",
        "loadCodeAssist project used without onboarding"
    );
    assert_eq!(e.state.onboard_calls.load(Ordering::SeqCst), 0);

    let mut c = stored(&e.gw, &id);
    c.expires_at = chrono::Utc::now().timestamp() - 10;
    e.gw.app.store.put("connection", &id, &c).unwrap();
    credentials::refresh(&e.gw.app, &mut c).await.unwrap();
    let forms = e.state.token_forms.lock().unwrap().clone();
    let refresh = forms.last().unwrap();
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "1//refresh-secret");
    assert_eq!(
        (
            refresh["client_id"].as_str(),
            refresh["client_secret"].as_str()
        ),
        (CLIENT_ID, CLIENT_SECRET)
    );
    let after = stored(&e.gw, &id);
    assert_eq!(after.api_key, "ya29.access-2");
    assert_eq!(
        after.refresh_token, "1//refresh-secret",
        "Google does not rotate; the refresh token is kept"
    );
    assert!(after.expires_at > chrono::Utc::now().timestamp() + 3000);
}

fn agy_file(
    dir: &std::path::Path,
    access: &str,
    expiry: &str,
    email: Option<&str>,
) -> std::path::PathBuf {
    let p = dir.join("antigravity-oauth-token");
    let mut v = json!({"token":{"access_token":access,"token_type":"Bearer","refresh_token":"1//agy-owned-refresh","expiry":expiry},"auth_method":"consumer"});
    if let Some(email) = email {
        v["id_token"] = json!(jwt(json!({"email":email,"sub":"1093"})));
    }
    std::fs::write(&p, v.to_string()).unwrap();
    p
}

#[tokio::test]
async fn agy_import_is_read_only_adopts_rotations_and_never_refreshes() {
    let _serial = SERIAL.lock().await;
    let e = env(Google {
        direct_project: Some("agy-proj".into()),
        ..Default::default()
    })
    .await;
    let src = tempfile::tempdir().unwrap();
    let soon = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let file = agy_file(src.path(), "ya29.agy-1", &soon, Some("person@example.com"));
    let r =
        e.gw.admin_send(
            Method::POST,
            "/api/import",
            json!({"source":"antigravity","path":file}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(!text.contains("ya29.") && !text.contains("agy-owned-refresh"));
    let v: Value = serde_json::from_str(&text).unwrap();
    let c = &v["connections"][0];
    assert_eq!(
        (c["kind"].as_str(), c["credential_source"].as_str()),
        (Some("antigravity"), Some("native_agy"))
    );
    assert_eq!(c["name"], "person@example.com");
    let id = c["id"].as_str().unwrap().to_string();
    // Reimport updates the same account.
    let again: Value =
        e.gw.admin_send(
            Method::POST,
            "/api/import",
            json!({"source":"antigravity","path":file}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again["connections"][0]["id"], id.as_str());

    // Expired: the gateway rereads the file and adopts a newer token written by agy.
    let mut row = stored(&e.gw, &id);
    row.expires_at = chrono::Utc::now().timestamp() - 5;
    e.gw.app.store.put("connection", &id, &row).unwrap();
    let later = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    agy_file(src.path(), "ya29.agy-2", &later, Some("person@example.com"));
    let before = std::fs::read(&file).unwrap();
    let mut c = stored(&e.gw, &id);
    credentials::refresh(&e.gw.app, &mut c).await.unwrap();
    assert_eq!(c.api_key, "ya29.agy-2");
    assert_eq!(c.credential_source, "native_agy");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        before,
        "the source file is never written"
    );
    assert!(
        e.state.token_forms.lock().unwrap().is_empty(),
        "agy's refresh token is never used by the gateway"
    );

    // Expired with nothing newer: a clear 401, still no token request.
    let mut row = stored(&e.gw, &id);
    row.expires_at = chrono::Utc::now().timestamp() - 5;
    e.gw.app.store.put("connection", &id, &row).unwrap();
    agy_file(
        src.path(),
        "ya29.agy-2",
        &(chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339(),
        Some("person@example.com"),
    );
    let mut c = stored(&e.gw, &id);
    let err = credentials::refresh(&e.gw.app, &mut c).await.unwrap_err();
    assert_eq!(err.status.as_u16(), 401);
    assert!(err.message.contains("Run agy once"), "{}", err.message);
    assert!(e.state.token_forms.lock().unwrap().is_empty());

    // The file now belongs to someone else: never adopted.
    agy_file(src.path(), "ya29.other", &later, Some("someone@else.com"));
    let mut c = stored(&e.gw, &id);
    let err = credentials::refresh(&e.gw.app, &mut c).await.unwrap_err();
    assert!(err.message.contains("different account"), "{}", err.message);
    assert_ne!(stored(&e.gw, &id).api_key, "ya29.other");

    // Signing in to the same Google account converts the import into an independent account.
    let done = sign_in(&e).await;
    assert_eq!(
        done["connection"]["id"],
        id.as_str(),
        "same identity across import and sign-in"
    );
    assert_eq!(done["connection"]["credential_source"], "oauth");
}

#[tokio::test]
async fn cliproxy_antigravity_files_and_default_agy_path_errors() {
    let _serial = SERIAL.lock().await;
    let e = env(Google::default()).await;
    let src = tempfile::tempdir().unwrap();
    let p = src.path().join("antigravity-a@b.c.json");
    std::fs::write(&p, json!({"type":"antigravity","email":"A@B.c","access_token":"ya29.cpa","refresh_token":"1//cpa","expired":(chrono::Utc::now()+chrono::Duration::hours(1)).to_rfc3339(),"project_id":"cpa-proj"}).to_string()).unwrap();
    let v: Value =
        e.gw.admin_send(
            Method::POST,
            "/api/import",
            json!({"source":"cliproxy","path":src.path()}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["imported"], 1, "{v}");
    let id = v["connections"][0]["id"].as_str().unwrap();
    let s = stored(&e.gw, id);
    assert_eq!(
        (
            s.kind.as_str(),
            s.credential_source.as_str(),
            s.account_id.as_str()
        ),
        ("antigravity", "cliproxy", "cpa-proj")
    );
    assert_eq!(s.account_identity, antigravity::identity_for_email("a@b.c"));
    // A missing file gives an actionable message.
    let r =
        e.gw.admin_send(
            Method::POST,
            "/api/import",
            json!({"source":"antigravity","path":src.path().join("nope")}),
        )
        .await;
    assert_eq!(r.status(), 400);
    let r =
        e.gw.admin_send(
            Method::POST,
            "/api/import",
            json!({"source":"antigravity","path":p}),
        )
        .await;
    assert_eq!(
        r.status(),
        400,
        "a CLIProxyAPI file is not an agy token file"
    );
}

#[tokio::test]
async fn quota_helper_project_persistence_and_catalog() {
    let _serial = SERIAL.lock().await;
    let e = env(Google {
        direct_project: Some("q-proj".into()),
        ..Default::default()
    })
    .await;
    let done = sign_in(&e).await;
    let id = done["connection"]["id"].as_str().unwrap().to_string();

    let mut c = stored(&e.gw, &id);
    let quota = antigravity::fetch_quota(&e.gw.app, &mut c).await.unwrap();
    let models: Vec<_> = quota
        .iter()
        .map(|q| (q.model.as_str(), q.remaining_fraction))
        .collect();
    assert_eq!(
        models,
        [
            ("claude-sonnet-4-6", Some(0.9)),
            ("gemini-3-flash", Some(0.4))
        ]
    );
    let public = serde_json::to_string(&quota).unwrap();
    assert!(
        !public.contains("ya29.") && !public.contains("q-proj"),
        "quota output carries no secrets"
    );

    // A forgotten project is rediscovered and persisted once.
    let mut row = stored(&e.gw, &id);
    row.account_id.clear();
    e.gw.app.store.put("connection", &id, &row).unwrap();
    let mut c = stored(&e.gw, &id);
    antigravity::ensure_project(&e.gw.app, &mut c)
        .await
        .unwrap();
    assert_eq!(c.account_id, "q-proj");
    assert_eq!(stored(&e.gw, &id).account_id, "q-proj");

    let mut c = stored(&e.gw, &id);
    let catalog = antigravity::catalog(&e.gw.app, &mut c).await.unwrap();
    assert!(
        catalog
            .iter()
            .any(|(m, name)| m == "gemini-3-flash" && name == "Gemini 3 Flash")
    );
    assert!(
        catalog
            .iter()
            .any(|(m, name)| m == "project-echo" && name == "q-proj"),
        "the stored project is sent"
    );
    assert!(!catalog.iter().any(|(m, _)| m == "chat_20706"));

    // A rejected credential is a dependency failure with a constant message.
    *e.state.accepted.lock().unwrap() = "something-else".into();
    let mut c = stored(&e.gw, &id);
    let err = antigravity::fetch_quota(&e.gw.app, &mut c)
        .await
        .unwrap_err();
    assert_eq!(err.status.as_u16(), 424);
    assert!(!err.message.contains(PROVIDER_KEY));
    let _ = &e.google;
}
