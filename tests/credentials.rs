//! Credential lifecycle and browser sign-in tests. Fake credential files and a loopback mock
//! token endpoint only: no real auth stores, Keychain or provider sign-in is touched.
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use switchyard::{
    app::{App, AppState},
    credentials, oauth,
    store::Connection,
};

/// OAuth endpoint overrides are process-global, so tests that use them run one at a time.
static OAUTH_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
/// Expiry computed by the code under test, allowing for a second boundary.
fn near(actual: i64, expected: i64) -> bool {
    (actual - expected).abs() <= 2
}
fn jwt(payload: Value) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(payload.to_string())
    )
}
fn codex_access(sub: &str, account: &str, exp: i64) -> String {
    jwt(
        json!({"exp":exp,"sub":sub,"https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_plan_type":"pro"}}),
    )
}
fn write(p: &Path, v: Value) {
    std::fs::write(p, v.to_string()).unwrap();
}
fn stored(app: &App, id: &str) -> Connection {
    app.store.get("connection", id).unwrap()
}
fn all(app: &App) -> Vec<Connection> {
    app.store.list("connection")
}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A loopback stand-in for a provider token endpoint.
#[derive(Clone, Default)]
struct Mock {
    hits: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    reply: Arc<Mutex<(u16, Value)>>,
}
impl Mock {
    async fn start() -> (Self, String) {
        let m = Mock::default();
        *m.reply.lock().unwrap() = (500, json!({}));
        let app = Router::new()
            .route(
                "/token",
                post(|State(m): State<Mock>, h: HeaderMap, b: Bytes| async move {
                    m.hits.lock().unwrap().push((h, b));
                    let (s, v) = m.reply.lock().unwrap().clone();
                    (StatusCode::from_u16(s).unwrap(), axum::Json(v))
                }),
            )
            .with_state(m.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", l.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        (m, url)
    }
    fn reply(&self, status: u16, v: Value) {
        *self.reply.lock().unwrap() = (status, v);
    }
    fn count(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
    fn body(&self, i: usize) -> String {
        String::from_utf8(self.hits.lock().unwrap()[i].1.to_vec()).unwrap()
    }
}

// ---------------------------------------------------------------------------------------------
// Source-owned (native / CLIProxyAPI) credentials: never rotated by the gateway
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn native_codex_adopts_newer_source_token_and_never_calls_the_network() {
    let _s = OAUTH_SERIAL.lock().await;
    let (m, url) = Mock::start().await;
    oauth::set_test_endpoints("codex", free_port(), &url, None);
    let (_d, app) = app();
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    let old = codex_access("auth0|maria", "ws-1", now() + 30);
    write(
        &file,
        json!({"tokens":{"access_token":old,"refresh_token":"rt-shared-1","account_id":"ws-1"}}),
    );
    let c = credentials::import(&app, "codex", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(c.credential_source, "native_codex");
    assert!(!c.account_identity.is_empty());
    assert!(near(c.expires_at, now() + 30));

    // The user edits configuration while a request still holds the old snapshot.
    let mut snapshot = stored(&app, &c.id);
    let mut edited = snapshot.clone();
    edited.name = "Work".into();
    edited.models = vec!["gpt-custom".into()];
    edited.enabled = false;
    app.store.put("connection", &c.id, &edited).unwrap();

    // Source not refreshed yet: clear 401, no network call, nothing changed.
    let e = credentials::refresh(&app, &mut snapshot).await.unwrap_err();
    assert_eq!(e.status.as_u16(), 401);
    assert!(e.message.contains("codex"), "{}", e.message);
    assert!(!e.message.contains("rt-shared"));

    // The Codex CLI refreshes its own file; the gateway adopts it read-only.
    let new = codex_access("auth0|maria", "ws-1", now() + 7200);
    write(
        &file,
        json!({"tokens":{"access_token":new,"refresh_token":"rt-shared-2","account_id":"ws-1"}}),
    );
    let before = std::fs::read(&file).unwrap();
    credentials::refresh(&app, &mut snapshot).await.unwrap();
    assert_eq!(snapshot.api_key, new);
    let s = stored(&app, &c.id);
    assert_eq!(s.api_key, new);
    assert_eq!(s.refresh_token, "rt-shared-2");
    assert!(near(s.expires_at, now() + 7200));
    assert_eq!(
        (s.name.as_str(), s.enabled),
        ("Work", false),
        "config preserved"
    );
    assert_eq!(s.models, vec!["gpt-custom".to_string()]);
    assert_eq!(s.credential_source, "native_codex");
    assert_eq!(std::fs::read(&file).unwrap(), before, "source untouched");
    assert_eq!(
        m.count(),
        0,
        "native credentials must never be refreshed by the gateway"
    );

    // A 401 on the fresh token with no newer source token: refuse, don't loop.
    let e = credentials::refresh_forced(&app, &mut snapshot)
        .await
        .unwrap_err();
    assert_eq!(e.status.as_u16(), 401);
    assert_eq!(m.count(), 0);
}

#[tokio::test]
async fn source_signed_in_to_another_account_is_not_adopted() {
    let (_d, app) = app();
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    write(
        &file,
        json!({"tokens":{"access_token":codex_access("auth0|a","ws",now()+10),"account_id":"ws"}}),
    );
    let mut c = credentials::import(&app, "codex", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    write(
        &file,
        json!({"tokens":{"access_token":codex_access("auth0|b","ws",now()+7200),"account_id":"ws"}}),
    );
    let e = credentials::refresh(&app, &mut c).await.unwrap_err();
    assert_eq!(e.status.as_u16(), 401);
    assert!(e.message.contains("different account"), "{}", e.message);
    assert!(near(stored(&app, &c.id).expires_at, now() + 10));
}

#[tokio::test]
async fn refresh_fails_cleanly_when_the_connection_was_deleted() {
    let (_d, app) = app();
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    write(
        &file,
        json!({"tokens":{"access_token":codex_access("auth0|a","ws",now()+5),"account_id":"ws"}}),
    );
    let mut c = credentials::import(&app, "codex", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    app.store.delete("connection", &c.id).unwrap();
    write(
        &file,
        json!({"tokens":{"access_token":codex_access("auth0|a","ws",now()+7200),"account_id":"ws"}}),
    );
    let e = credentials::refresh(&app, &mut c).await.unwrap_err();
    assert_eq!(e.status.as_u16(), 409);
    assert!(
        all(&app).is_empty(),
        "a refresh must not resurrect a deleted account"
    );
}

#[tokio::test]
async fn shared_workspace_users_are_distinct_and_reimport_is_stable() {
    let (_d, app) = app();
    let src = tempfile::tempdir().unwrap();
    let (a, b) = (src.path().join("a.json"), src.path().join("b.json"));
    write(
        &a,
        json!({"tokens":{"access_token":codex_access("auth0|alice","team-ws",now()+7200),"account_id":"team-ws"}}),
    );
    write(
        &b,
        json!({"tokens":{"access_token":codex_access("auth0|bob","team-ws",now()+7200),"account_id":"team-ws"}}),
    );
    let ca = credentials::import(&app, "codex", Some(a.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    let cb = credentials::import(&app, "codex", Some(b.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    assert_ne!(ca.id, cb.id, "same workspace, different people");
    assert_eq!(all(&app).len(), 2);
    // Alice's CLI refreshed: same identity, new token, same connection.
    write(
        &a,
        json!({"tokens":{"access_token":codex_access("auth0|alice","team-ws",now()+9000),"account_id":"team-ws"}}),
    );
    let again = credentials::import(&app, "codex", Some(a.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(again.id, ca.id);
    assert!(near(stored(&app, &ca.id).expires_at, now() + 9000));
    assert_eq!(all(&app).len(), 2);
}

#[tokio::test]
async fn opaque_claude_token_rotation_keeps_one_connection() {
    let (_d, app) = app();
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join(".credentials.json");
    let exp = (now() + 3600) * 1000;
    write(
        &file,
        json!({"claudeAiOauth":{"accessToken":"sk-ant-oat-1","refreshToken":"sk-ant-ort-1","expiresAt":exp}}),
    );
    let c = credentials::import(&app, "claude", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(c.credential_source, "native_claude");
    assert_eq!(c.expires_at, exp / 1000);
    write(
        &file,
        json!({"claudeAiOauth":{"accessToken":"sk-ant-oat-2","refreshToken":"sk-ant-ort-2","expiresAt":exp + 3_600_000}}),
    );
    let again = credentials::import(&app, "claude", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(again.id, c.id);
    assert_eq!(all(&app).len(), 1);
    assert_eq!(stored(&app, &c.id).api_key, "sk-ant-oat-2");
}

#[tokio::test]
async fn cliproxy_expiry_formats_and_email_identity() {
    let (_d, app) = app();
    let src = tempfile::tempdir().unwrap();
    let p = src.path();
    let rfc = chrono::DateTime::from_timestamp(now() + 5000, 0)
        .unwrap()
        .to_rfc3339();
    write(
        &p.join("claude.json"),
        json!({"type":"claude","email":"Maria@Example.com","access_token":"sk-ant-oat-a","refresh_token":"r","expired":rfc}),
    );
    write(
        &p.join("codex.json"),
        json!({"type":"codex","email":"m@example.com","access_token":"opaque","account_id":"ws","expires_at":now()+6000}),
    );
    write(
        &p.join("codex-ms.json"),
        json!({"type":"codex","email":"n@example.com","access_token":"opaque2","account_id":"ws","expiresAt":(now()+7000)*1000}),
    );
    let cs = credentials::import(&app, "cliproxy", Some(p.to_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(cs.len(), 3);
    let by = |email: &str| cs.iter().find(|c| c.name == email).unwrap().clone();
    assert!(near(by("Maria@Example.com").expires_at, now() + 5000));
    assert!(near(by("m@example.com").expires_at, now() + 6000));
    assert!(near(by("n@example.com").expires_at, now() + 7000));
    assert!(cs.iter().all(|c| c.credential_source == "cliproxy"));

    // CLIProxyAPI rotated the Claude token in place: same account, no duplicate.
    write(
        &p.join("claude.json"),
        json!({"type":"claude","email":"maria@example.com","access_token":"sk-ant-oat-b","refresh_token":"r2","expired":rfc}),
    );
    credentials::import(&app, "cliproxy", Some(p.to_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(all(&app).len(), 3);
    assert_eq!(
        stored(&app, &by("Maria@Example.com").id).api_key,
        "sk-ant-oat-b"
    );
}

#[tokio::test]
async fn reimport_does_not_regress_to_an_older_token() {
    let (_d, app) = app();
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("c.json");
    write(
        &file,
        json!({"type":"claude","email":"x@example.com","access_token":"new","expires_at":now()+9000}),
    );
    let c = credentials::import(&app, "cliproxy", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    write(
        &file,
        json!({"type":"claude","email":"x@example.com","access_token":"old","expires_at":now()+100}),
    );
    credentials::import(&app, "cliproxy", Some(file.to_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(stored(&app, &c.id).api_key, "new");
}

#[tokio::test]
async fn api_key_and_legacy_rows_never_refresh() {
    let (_d, app) = app();
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    write(&file, json!({"OPENAI_API_KEY":"sk-fake"}));
    let mut c = credentials::import(&app, "codex", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(c.credential_source, "api_key");
    credentials::refresh(&app, &mut c).await.unwrap();
    assert_eq!(
        credentials::refresh_forced(&app, &mut c)
            .await
            .unwrap_err()
            .status
            .as_u16(),
        401
    );

    // A row imported before ownership tracking: never rotate a possibly shared token.
    let mut legacy = c.clone();
    legacy.id = "legacy".into();
    legacy.kind = "codex".into();
    legacy.oauth = true;
    legacy.credential_source.clear();
    legacy.refresh_token = "rt".into();
    legacy.expires_at = now() - 10;
    app.store.put("connection", "legacy", &legacy).unwrap();
    let e = credentials::refresh(&app, &mut legacy).await.unwrap_err();
    assert_eq!(e.status.as_u16(), 401);
    assert!(e.message.contains("Reimport"), "{}", e.message);
}

// ---------------------------------------------------------------------------------------------
// Independent browser sign-in (PKCE) and gateway-owned refresh
// ---------------------------------------------------------------------------------------------

fn query(url: &str) -> std::collections::HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}
async fn callback(port: u16, path: &str, q: &str) -> (u16, String) {
    let r = reqwest::get(format!("http://127.0.0.1:{port}{path}?{q}"))
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

#[tokio::test]
async fn codex_sign_in_completes_with_pkce_and_gateway_owns_refresh() {
    let _s = OAUTH_SERIAL.lock().await;
    let (m, url) = Mock::start().await;
    let port = free_port();
    oauth::set_test_endpoints("codex", port, &url, None);
    oauth::set_test_ttl(None);
    let (_d, app) = app();
    let v = oauth::start(app.clone(), "codex").await.unwrap();
    assert_eq!(v["status"], "pending");
    assert_eq!(v["expires_in_seconds"], 300);
    let id = v["id"].as_str().unwrap().to_string();
    let auth = v["authorization_url"].as_str().unwrap();
    assert!(auth.starts_with("https://auth.openai.com/oauth/authorize?"));
    let q = query(auth);
    assert_eq!(q["client_id"], "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(
        q["redirect_uri"],
        format!("http://localhost:{port}/auth/callback")
    );
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["state"].len() >= 64);
    let state = q["state"].clone();

    // Forged or foreign requests do not end the flow.
    let (s, _) = callback(port, "/auth/callback", "code=c&state=wrong").await;
    assert_eq!(s, 400);
    let r = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/auth/callback?code=c&state={state}"
        ))
        .header("host", format!("evil.example:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(oauth::status(&app, &id).unwrap()["status"], "pending");
    assert_eq!(m.count(), 0);

    let access = codex_access("auth0|maria", "ws-9", now() + 864000);
    let id_token = jwt(
        json!({"sub":"auth0|maria","email":"maria@example.com","https://api.openai.com/auth":{"chatgpt_account_id":"ws-9","chatgpt_plan_type":"plus"}}),
    );
    m.reply(200, json!({"access_token":access,"refresh_token":"rt-owned-1","id_token":id_token,"expires_in":864000}));
    let (s, page) = callback(
        port,
        "/auth/callback",
        &format!("code=the-code&state={state}"),
    )
    .await;
    assert_eq!(s, 200, "{page}");
    assert!(page.contains("control room"));
    assert!(!page.contains("the-code") && !page.contains("rt-owned"));

    // PKCE: the verifier sent to the token endpoint hashes to the published challenge.
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(m.body(0).as_bytes())
            .into_owned()
            .collect();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "the-code");
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(form["code_verifier"].as_bytes()));
    assert_eq!(challenge, q["code_challenge"]);

    let st = oauth::status(&app, &id).unwrap();
    assert_eq!(st["status"], "complete");
    let text = st.to_string();
    assert!(!text.contains("the-code") && !text.contains(&access) && !text.contains("rt-owned"));
    let cid = st["connection"]["id"].as_str().unwrap().to_string();
    let c = stored(&app, &cid);
    assert_eq!(
        (
            c.kind.as_str(),
            c.credential_source.as_str(),
            c.name.as_str()
        ),
        ("codex", "oauth", "maria@example.com")
    );
    assert_eq!(c.account_id, "ws-9");
    assert!(c.supports_websocket && c.models.contains(&"gpt-6.1-sol".to_string()));

    // The callback port is released once the flow ends.
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("callback port released");
    // Replays are refused.
    assert_eq!(callback_status(port, &state).await, None);

    // Native import of the same person must not replace the independent token family.
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("auth.json");
    write(
        &file,
        json!({"tokens":{"access_token":codex_access("auth0|maria","ws-9",now()+3600),"refresh_token":"rt-native","account_id":"ws-9"}}),
    );
    let again = credentials::import(&app, "codex", Some(file.to_str().unwrap()))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(again.id, cid);
    let c = stored(&app, &cid);
    assert_eq!(
        (c.refresh_token.as_str(), c.credential_source.as_str()),
        ("rt-owned-1", "oauth")
    );

    // Gateway-owned refresh: under the lock, patches only token fields.
    let mut expiring = c.clone();
    expiring.expires_at = now() + 10;
    expiring.name = "Renamed".into();
    app.store.put("connection", &cid, &expiring).unwrap();
    let mut snapshot = expiring.clone();
    snapshot.name = "stale".into();
    let fresh = codex_access("auth0|maria", "ws-9", now() + 864000 + 5);
    m.reply(
        200,
        json!({"access_token":fresh,"refresh_token":"rt-owned-2","id_token":id_token}),
    );
    credentials::refresh(&app, &mut snapshot).await.unwrap();
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(m.body(1).as_bytes())
            .into_owned()
            .collect();
    assert_eq!(
        (form["grant_type"].as_str(), form["refresh_token"].as_str()),
        ("refresh_token", "rt-owned-1")
    );
    let c = stored(&app, &cid);
    assert_eq!(
        (
            c.api_key.as_str(),
            c.refresh_token.as_str(),
            c.name.as_str()
        ),
        (fresh.as_str(), "rt-owned-2", "Renamed")
    );
    assert_eq!(
        snapshot.name, "Renamed",
        "caller sees the latest configuration"
    );

    // A revoked refresh token yields a bounded, secret-free 401.
    m.reply(
        400,
        json!({"error":"invalid_grant","error_description":"rt-owned-2 reused"}),
    );
    let mut snapshot = c.clone();
    let e = credentials::refresh_forced(&app, &mut snapshot)
        .await
        .unwrap_err();
    assert_eq!(e.status.as_u16(), 401);
    assert!(
        !e.message.contains("rt-owned") && e.message.contains("Sign in again"),
        "{}",
        e.message
    );
    assert_eq!(stored(&app, &cid).refresh_token, "rt-owned-2");
}

async fn callback_status(port: u16, state: &str) -> Option<u16> {
    reqwest::get(format!(
        "http://127.0.0.1:{port}/auth/callback?code=x&state={state}"
    ))
    .await
    .ok()
    .map(|r| r.status().as_u16())
}

#[tokio::test]
async fn claude_sign_in_uses_account_uuid_and_json_exchange() {
    let _s = OAUTH_SERIAL.lock().await;
    let (m, url) = Mock::start().await;
    let port = free_port();
    oauth::set_test_endpoints("claude", port, &url, Some(&url));
    oauth::set_test_ttl(None);
    let (_d, app) = app();
    let v = oauth::start(app.clone(), "claude").await.unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let q = query(v["authorization_url"].as_str().unwrap());
    assert!(
        v["authorization_url"]
            .as_str()
            .unwrap()
            .starts_with("https://claude.ai/oauth/authorize?")
    );
    assert_eq!(q["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(
        q["redirect_uri"],
        format!("http://localhost:{port}/callback")
    );
    m.reply(200, json!({"access_token":"sk-ant-oat-x","refresh_token":"sk-ant-ort-x","expires_in":28800,"account":{"uuid":"acc-uuid-1","email_address":"m@example.com"}}));
    // Claude's manual form: code#state pasted by the user (gateway on another machine).
    let st = oauth::submit_callback(&app, &id, &format!("pasted-code#{}", q["state"]))
        .await
        .unwrap();
    assert_eq!(st["status"], "complete");
    let body: Value = serde_json::from_str(&m.body(0)).unwrap();
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "pasted-code");
    assert_eq!(body["state"], q["state"].as_str());
    let c = stored(&app, st["connection"]["id"].as_str().unwrap());
    assert_eq!(
        (c.kind.as_str(), c.credential_source.as_str()),
        ("anthropic", "oauth")
    );
    assert!(near(c.expires_at, now() + 28800));
    // Same account again via a second sign-in: updated in place, not duplicated.
    let v = oauth::start(app.clone(), "claude").await.unwrap();
    let q = query(v["authorization_url"].as_str().unwrap());
    m.reply(200, json!({"access_token":"sk-ant-oat-y","refresh_token":"sk-ant-ort-y","expires_in":28800,"account":{"uuid":"acc-uuid-1"}}));
    let (s, _) = callback(port, "/callback", &format!("code=c2&state={}", q["state"])).await;
    assert_eq!(s, 200);
    assert_eq!(all(&app).len(), 1);
    assert_eq!(stored(&app, &c.id).api_key, "sk-ant-oat-y");
}

#[tokio::test]
async fn sign_in_errors_expiry_cancellation_and_port_conflicts() {
    let _s = OAUTH_SERIAL.lock().await;
    let (m, url) = Mock::start().await;
    let (_d, app) = app();
    assert_eq!(
        oauth::start(app.clone(), "gemini")
            .await
            .unwrap_err()
            .status
            .as_u16(),
        400
    );
    assert_eq!(
        oauth::status(&app, "nope").unwrap_err().status.as_u16(),
        404
    );

    // Provider error with a valid state ends the flow and frees the port.
    let port = free_port();
    oauth::set_test_endpoints("codex", port, &url, None);
    oauth::set_test_ttl(None);
    let v = oauth::start(app.clone(), "codex").await.unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let state = query(v["authorization_url"].as_str().unwrap())["state"].clone();
    let (s, _) = callback(
        port,
        "/auth/callback",
        &format!("error=access_denied&state={state}"),
    )
    .await;
    assert_eq!(s, 400);
    let st = oauth::status(&app, &id).unwrap();
    assert_eq!(st["status"], "error");
    assert!(st["message"].as_str().unwrap().contains("denied"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("port released after error");

    // Token endpoint failure: error status, safe message, no provider body echoed.
    m.reply(400, json!({"error":"invalid_grant","secret":"leak-me"}));
    let v = oauth::start(app.clone(), "codex").await.unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let state = query(v["authorization_url"].as_str().unwrap())["state"].clone();
    let (s, page) = callback(port, "/auth/callback", &format!("code=c&state={state}")).await;
    assert_eq!(s, 502);
    assert!(!page.contains("leak-me"));
    let st = oauth::status(&app, &id).unwrap().to_string();
    assert!(st.contains("\"error\"") && !st.contains("leak-me"));

    // A newer sign-in replaces an older one for the same provider.
    let first = oauth::start(app.clone(), "codex").await.unwrap();
    let second = oauth::start(app.clone(), "codex").await.unwrap();
    assert_eq!(
        oauth::status(&app, first["id"].as_str().unwrap()).unwrap()["status"],
        "error"
    );
    assert_eq!(
        oauth::status(&app, second["id"].as_str().unwrap()).unwrap()["status"],
        "pending"
    );
    let st = oauth::cancel(&app, second["id"].as_str().unwrap()).unwrap();
    assert_eq!(st["status"], "error");

    // TTL: the flow expires and its listener is released.
    oauth::set_test_ttl(Some(Duration::from_millis(200)));
    let v = oauth::start(app.clone(), "codex").await.unwrap();
    assert_eq!(v["expires_in_seconds"], 0);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        oauth::status(&app, v["id"].as_str().unwrap()).unwrap()["status"],
        "expired"
    );
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("port released after expiry");
    oauth::set_test_ttl(None);

    // Something else owns the callback port: clear error naming the port.
    let blocker = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let e = oauth::start(app.clone(), "codex").await.unwrap_err();
    assert_eq!(e.status.as_u16(), 409);
    assert!(e.message.contains(&port.to_string()), "{}", e.message);
    drop(blocker);
}
