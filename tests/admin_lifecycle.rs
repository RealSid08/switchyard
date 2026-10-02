//! Admin session lifecycle and browser sign-in (OAuth) over the real HTTP routes.
//! Token exchange goes to a loopback mock through the crate's test hook; no real provider is
//! contacted and no local CLI credentials are read.

mod support;

use axum::{Router, routing::post};
use base64::Engine;
use futures_util::StreamExt;
use reqwest::Method;
use serde_json::{Value, json};
use sha2::Digest;
use std::collections::HashMap;
use support::*;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

async fn setup() -> (tempfile::TempDir, Gateway) {
    let dir = tempfile::tempdir().unwrap();
    let gw = Gateway::start(dir.path()).await;
    (dir, gw)
}

/// Mint a session the way the local dashboard does (same-origin Fetch Metadata, loopback Host).
async fn browser_session(gw: &Gateway) -> (String, String) {
    let r = gw
        .http
        .post(gw.url("/api/session"))
        .header("sec-fetch-site", "same-origin")
        .header("origin", format!("http://127.0.0.1:{}", gw.port))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let header = r.headers()["set-cookie"].to_str().unwrap().to_string();
    (header.split(';').next().unwrap().to_string(), header)
}

/// Mint a session with the admin token (remote administration).
async fn token_session(gw: &Gateway) -> String {
    let r = gw
        .http
        .post(gw.url("/api/session"))
        .bearer_auth(&gw.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn overview_with(gw: &Gateway, cookie: &str) -> u16 {
    gw.http
        .get(gw.url("/api/overview"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn logout(gw: &Gateway, cookie: Option<&str>, extra: &[(&str, String)]) -> reqwest::Response {
    let mut r = gw.http.delete(gw.url("/api/session"));
    if let Some(c) = cookie {
        r = r.header("cookie", c);
    }
    for (k, v) in extra {
        r = r.header(*k, v);
    }
    r.send().await.unwrap()
}

// ---------------------------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn sessions_are_unique_and_logout_revokes_only_that_session() {
    let (_d, gw) = setup().await;
    let (a, header) = browser_session(&gw).await;
    let b = token_session(&gw).await;
    assert_ne!(a, b, "each sign-in gets its own session");
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/api", "Max-Age=43200"] {
        assert!(header.contains(attr), "{attr} missing: {header}");
    }
    for c in [&a, &b] {
        assert!(
            !c.contains(&gw.admin),
            "session cookie must not be the admin token"
        );
        assert!(
            c.len() > "sy_session=".len() + 32,
            "session token too short: {c}"
        );
    }
    assert_eq!(overview_with(&gw, &a).await, 200);
    assert_eq!(overview_with(&gw, &b).await, 200);

    // Logout clears the browser cookie and revokes that session server-side.
    let r = logout(&gw, Some(&a), &[]).await;
    assert_eq!(r.status(), 200);
    let cleared = r.headers()["set-cookie"].to_str().unwrap();
    assert!(
        cleared.starts_with("sy_session=;") && cleared.contains("Max-Age=0"),
        "{cleared}"
    );
    assert_eq!(r.headers()["cache-control"], "no-store");
    assert_eq!(
        overview_with(&gw, &a).await,
        401,
        "logged-out cookie still works"
    );
    assert_eq!(
        overview_with(&gw, &b).await,
        200,
        "logout must not end other sessions"
    );
    assert_eq!(
        gw.admin_get("/api/overview").await.status(),
        200,
        "admin token is unaffected"
    );

    // Idempotent, and harmless without a cookie.
    assert_eq!(logout(&gw, Some(&a), &[]).await.status(), 200);
    assert_eq!(logout(&gw, None, &[]).await.status(), 200);
    assert_eq!(overview_with(&gw, &b).await, 200);

    // A cross-site page or a rebinding Host cannot log the user out.
    let r = logout(&gw, Some(&b), &[("origin", "https://evil.example".into())]).await;
    assert_eq!(r.status(), 403);
    let r = logout(&gw, Some(&b), &[("sec-fetch-site", "cross-site".into())]).await;
    assert_eq!(r.status(), 403);
    let r = logout(
        &gw,
        Some(&b),
        &[("host", format!("evil.example:{}", gw.port))],
    )
    .await;
    assert_eq!(r.status(), 403);
    assert_eq!(overview_with(&gw, &b).await, 200);
}

#[tokio::test]
async fn session_minting_rules() {
    let (_d, gw) = setup().await;
    let mint = |headers: Vec<(&'static str, String)>| {
        let mut r = gw.http.post(gw.url("/api/session"));
        for (k, v) in headers {
            r = r.header(k, v);
        }
        r.send()
    };
    // No proof of locality or token.
    assert_eq!(mint(vec![]).await.unwrap().status(), 401);
    assert_eq!(
        mint(vec![("authorization", "Bearer sy_admin_wrong".into())])
            .await
            .unwrap()
            .status(),
        401
    );
    // Cross-site bootstrap is refused outright.
    assert_eq!(
        mint(vec![("sec-fetch-site", "cross-site".into())])
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        mint(vec![
            ("origin", "https://evil.example".into()),
            ("authorization", format!("Bearer {}", gw.admin))
        ])
        .await
        .unwrap()
        .status(),
        403
    );
    // Same-origin metadata through a non-loopback Host (DNS rebinding) is not local.
    let r = mint(vec![
        ("sec-fetch-site", "same-origin".into()),
        ("host", format!("evil.example:{}", gw.port)),
    ])
    .await
    .unwrap();
    assert_eq!(r.status(), 401);
    assert!(r.headers().get("set-cookie").is_none());
    // Case-insensitive Bearer scheme is accepted for the admin token.
    assert_eq!(
        mint(vec![("authorization", format!("bearer {}", gw.admin))])
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn session_cookie_is_scoped_to_administration() {
    let (_d, gw) = setup().await;
    let (cookie, _) = browser_session(&gw).await;
    // A dashboard session is not a client API key.
    let r = gw
        .http
        .get(gw.url("/v1/models"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // The live events socket accepts the session, and refuses it after logout.
    let mut req = gw.ws_url("/api/events").into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("events socket with session");
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.to_text().unwrap().contains("\"overview\""));
    drop(ws);
    logout(&gw, Some(&cookie), &[]).await;
    let mut req = gw.ws_url("/api/events").into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    assert!(
        matches!(&err, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 401),
        "{err:?}"
    );
}

#[tokio::test]
async fn session_store_is_bounded_and_evicts_the_oldest() {
    let (_d, gw) = setup().await;
    let first = token_session(&gw).await;
    let second = token_session(&gw).await;
    for _ in 0..254 {
        token_session(&gw).await;
    }
    // 256 live sessions: all still valid.
    assert_eq!(overview_with(&gw, &first).await, 200);
    let newest = token_session(&gw).await;
    assert_eq!(
        overview_with(&gw, &first).await,
        401,
        "the 257th session evicts the oldest"
    );
    assert_eq!(overview_with(&gw, &second).await, 200);
    assert_eq!(overview_with(&gw, &newest).await, 200);
}

// ---------------------------------------------------------------------------------------------
// Browser sign-in over HTTP
// ---------------------------------------------------------------------------------------------

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

fn jwt(payload: Value) -> String {
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(payload.to_string())
    )
}

const ACCESS_MARK: &str = "ACCESS-SECRET-a91";
const REFRESH_MARK: &str = "rt-REFRESH-SECRET-b72";

/// Codex token endpoint mock: issues a JWT access token with workspace and plan claims.
fn codex_token_endpoint() -> Router {
    Router::new().route(
        "/token",
        post(|| async {
            let exp = chrono::Utc::now().timestamp() + 3600;
            let auth = json!({"chatgpt_account_id":"acct-oauth-1","chatgpt_plan_type":"plus","chatgpt_user_id":"user-oauth-1"});
            json_response(
                200,
                json!({
                    "access_token": jwt(json!({"exp":exp,"sub":"user-oauth-1","https://api.openai.com/auth":auth,"mark":ACCESS_MARK})),
                    "refresh_token": REFRESH_MARK,
                    "id_token": jwt(json!({"sub":"user-oauth-1","email":"oauth@example.com","https://api.openai.com/auth":auth})),
                    "expires_in": 3600
                }),
            )
        }),
    )
}

fn form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[tokio::test]
async fn oauth_routes_require_admin_and_same_origin() {
    let (_d, gw) = setup().await;
    let r = gw
        .http
        .post(gw.url("/api/oauth/start"))
        .json(&json!({"provider":"codex"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    for (method, path) in [
        (Method::GET, "/api/oauth/abc"),
        (Method::DELETE, "/api/oauth/abc"),
        (Method::POST, "/api/oauth/abc/callback"),
    ] {
        let r = gw
            .http
            .request(method.clone(), gw.url(path))
            .json(&json!({"input":"x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "{method} {path}");
    }
    let r = gw
        .http
        .post(gw.url("/api/oauth/start"))
        .bearer_auth(&gw.admin)
        .header("origin", "https://evil.example")
        .json(&json!({"provider":"codex"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "cross-site pages cannot start sign-ins");
    let r = gw
        .admin_send(
            Method::POST,
            "/api/oauth/start",
            json!({"provider":"gemini"}),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        gw.admin_get("/api/oauth/does-not-exist").await.status(),
        404
    );
}

#[tokio::test]
async fn codex_sign_in_over_http_is_owned_by_the_starting_gateway() {
    let (_d1, gw) = setup().await;
    let (_d2, other) = setup().await;
    let token = Upstream::start(codex_token_endpoint()).await;
    let port = free_port();
    switchyard::oauth::set_test_endpoints("codex", port, &format!("{}/token", token.base()), None);
    switchyard::oauth::set_test_ttl(None);

    let r = gw
        .admin_send(
            Method::POST,
            "/api/oauth/start",
            json!({"provider":"codex"}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let started: Value = r.json().await.unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    assert_eq!(started["status"], "pending");
    assert_eq!(started["expires_in_seconds"], 300);
    let auth_url = started["authorization_url"].as_str().unwrap();
    assert!(
        auth_url.starts_with("https://auth.openai.com/oauth/authorize?"),
        "{auth_url}"
    );
    let q = query(auth_url);
    assert_eq!(
        q["redirect_uri"],
        format!("http://localhost:{port}/auth/callback")
    );
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["response_type"], "code");
    let state = q["state"].clone();
    assert!(state.len() >= 32);
    assert!(started.get("code_verifier").is_none() && !started.to_string().contains("verifier\""));

    // Another gateway in the same process cannot read, cancel or complete this sign-in.
    let path = format!("/api/oauth/{id}");
    assert_eq!(other.admin_get(&path).await.status(), 404);
    assert_eq!(
        other
            .admin_send(Method::DELETE, &path, json!({}))
            .await
            .status(),
        404
    );
    let callback = format!("http://localhost:{port}/auth/callback?code=code-123&state={state}");
    let r = other
        .admin_send(
            Method::POST,
            &format!("{path}/callback"),
            json!({"input":callback}),
        )
        .await;
    assert_eq!(r.status(), 404);
    assert_eq!(
        token.count(),
        0,
        "foreign gateway must not exchange the code"
    );
    assert_eq!(gw.admin_json(&path).await["status"], "pending");

    // Forged or malformed pastes do not end the sign-in.
    let forged = format!("http://localhost:{port}/auth/callback?code=code-123&state=wrong-state");
    let r = gw
        .admin_send(
            Method::POST,
            &format!("{path}/callback"),
            json!({"input":forged}),
        )
        .await;
    assert_eq!(r.status(), 400);
    let r = gw
        .admin_send(
            Method::POST,
            &format!("{path}/callback"),
            json!({"input":"   "}),
        )
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(gw.admin_json(&path).await["status"], "pending");
    assert_eq!(token.count(), 0);

    // The real paste completes it.
    let r = gw
        .admin_send(
            Method::POST,
            &format!("{path}/callback"),
            json!({"input":callback}),
        )
        .await;
    let text = r.text().await.unwrap();
    assert!(
        !text.contains(ACCESS_MARK) && !text.contains(REFRESH_MARK) && !text.contains("code-123"),
        "{text}"
    );
    let done: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(done["status"], "complete", "{done}");
    let conn = &done["connection"];
    assert_eq!(conn["kind"], "codex");
    assert_eq!(conn["credential_source"], "oauth");
    assert_eq!(conn["credential_present"], true);
    assert_eq!(conn["name"], "oauth@example.com");

    // Exchange used PKCE correctly.
    let sent = form(&token.requests()[0].body);
    assert_eq!(sent["grant_type"], "authorization_code");
    assert_eq!(sent["code"], "code-123");
    assert_eq!(
        sent["redirect_uri"],
        format!("http://localhost:{port}/auth/callback")
    );
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(sent["code_verifier"].as_bytes()));
    assert_eq!(challenge, q["code_challenge"]);

    // Replaying the callback does not exchange again; status stays complete and secret-free.
    let r = gw
        .admin_send(
            Method::POST,
            &format!("{path}/callback"),
            json!({"input":callback}),
        )
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(token.count(), 1);
    let status = gw.admin_get(&path).await.text().await.unwrap();
    assert!(
        !status.contains(ACCESS_MARK) && !status.contains(REFRESH_MARK),
        "{status}"
    );

    // The account belongs to this gateway only, with no secrets in its listing.
    let listed = gw.admin_get("/api/connections").await.text().await.unwrap();
    assert!(
        !listed.contains("acct-oauth-1")
            && !listed.contains(ACCESS_MARK)
            && !listed.contains(REFRESH_MARK),
        "{listed}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&listed)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(other.admin_json("/api/connections").await, json!([]));
    // Gateway-owned token family is stored for later refresh.
    let stored: switchyard::store::Connection = gw
        .app
        .store
        .get("connection", conn["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(stored.refresh_token, REFRESH_MARK);
    assert_eq!(stored.account_id, "acct-oauth-1");
    assert!(
        stored.source_path.is_empty(),
        "browser sign-in has no external source"
    );
}

#[tokio::test]
async fn claude_sign_in_cancel_callback_listener_and_restart() {
    let (dir, gw) = setup().await;
    let token = Upstream::start(
        Router::new().route("/token", post(|| async { json_response(500, json!({})) })),
    )
    .await;
    let port = free_port();
    let url = format!("{}/token", token.base());
    switchyard::oauth::set_test_endpoints("claude", port, &url, Some(&url));
    switchyard::oauth::set_test_ttl(None);

    // Cancel: ends the sign-in, closes the listener, and later callbacks are refused.
    let started = gw
        .admin_send(
            Method::POST,
            "/api/oauth/start",
            json!({"provider":"claude"}),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    let q = query(started["authorization_url"].as_str().unwrap());
    assert_eq!(
        q["redirect_uri"],
        format!("http://localhost:{port}/callback")
    );
    // The loopback listener rejects forged state and non-loopback Host without ending the flow.
    let listener = client();
    let r = listener
        .get(format!(
            "http://127.0.0.1:{port}/callback?code=c&state=forged"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let page = r.text().await.unwrap();
    assert!(
        !page.contains("forged"),
        "callback page must not reflect input"
    );
    let r = listener
        .get(format!(
            "http://127.0.0.1:{port}/callback?code=c&state={}",
            q["state"]
        ))
        .header("host", format!("evil.example:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        gw.admin_json(&format!("/api/oauth/{id}")).await["status"],
        "pending"
    );
    assert_eq!(token.count(), 0);

    let r = gw
        .admin_send(Method::DELETE, &format!("/api/oauth/{id}"), json!({}))
        .await;
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "error");
    assert!(v["message"].as_str().unwrap().contains("cancelled"), "{v}");
    let r = gw
        .admin_send(
            Method::POST,
            &format!("/api/oauth/{id}/callback"),
            json!({"input":format!("code#{}", q["state"])}),
        )
        .await;
    assert_eq!(r.status(), 409);
    assert_eq!(token.count(), 0);
    let mut closed = false;
    for _ in 0..50 {
        if listener
            .get(format!("http://127.0.0.1:{port}/callback"))
            .send()
            .await
            .is_err()
        {
            closed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(closed, "callback listener should close after cancellation");

    // Provider failure during exchange ends the sign-in with a generic message.
    let started = gw
        .admin_send(
            Method::POST,
            "/api/oauth/start",
            json!({"provider":"claude"}),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    let state = query(started["authorization_url"].as_str().unwrap())["state"].clone();
    let r = gw
        .admin_send(
            Method::POST,
            &format!("/api/oauth/{id}/callback"),
            json!({"input":format!("pasted-code#{state}")}),
        )
        .await;
    assert_eq!(r.status(), 502);
    assert_eq!(token.count(), 1);
    assert_eq!(
        gw.admin_json(&format!("/api/oauth/{id}")).await["status"],
        "error"
    );
    assert_eq!(gw.admin_json("/api/connections").await, json!([]));

    // Pending sign-ins live in memory only. A restarted process is a new instance, and
    // `codex_sign_in_over_http_is_owned_by_the_starting_gateway` shows another instance gets 404.
    // (An in-process restart cannot model this: the pending listener keeps the old instance and
    // its data-directory lock alive until the sign-in ends.)
    drop(dir);
}
