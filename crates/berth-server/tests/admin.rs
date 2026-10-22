//! Integration tests of the web admin (setup wizard, login, sessions, CSRF, settings, users,
//! connect, devices, storage) and of the device API it drives (pairing, refresh, revoke,
//! storage profiles).

use std::{net::SocketAddr, path::Path};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode, header},
};
use berth_server::{
    AppState,
    media::fixtures,
    service::{auth, pairing},
};
use http_body_util::BodyExt;
use sha2::Digest;
use tower::ServiceExt;

const PASSWORD: &str = "harbour lights";

async fn start(dir: &Path) -> AppState {
    // Server errors show up in the output of a failing test.
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let config = dir.join("berth.toml");
    std::fs::write(
        &config,
        format!("data_dir = '{}'\n", dir.join("data").display()),
    )
    .unwrap();
    berth_server::bootstrap(Some(&config)).await.unwrap()
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn location(&self) -> Option<&str> {
        self.headers
            .get(header::LOCATION)
            .or_else(|| self.headers.get("hx-redirect"))
            .map(|v| v.to_str().unwrap())
    }

    /// Session token from `Set-Cookie`.
    fn session(&self) -> Option<String> {
        let cookie = self.headers.get(header::SET_COOKIE)?.to_str().unwrap();
        let value = cookie.strip_prefix("berth_session=")?.split(';').next()?;
        (!value.is_empty()).then(|| value.to_owned())
    }
}

async fn send(state: &AppState, req: Request<Body>) -> Reply {
    let mut req = req;
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50_000))));
    let response = berth_server::router(state.clone())
        .oneshot(req)
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn get(uri: &str, session: Option<&str>) -> Request<Body> {
    let mut req = Request::get(uri);
    if let Some(token) = session {
        req = req.header(header::COOKIE, format!("berth_session={token}"));
    }
    req.body(Body::empty()).unwrap()
}

fn post(uri: &str, session: Option<&str>, form: &[(&str, &str)]) -> Request<Body> {
    let body = form
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}={}",
                v.replace(' ', "+").replace(':', "%3A").replace('/', "%2F")
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let mut req = Request::post(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("sec-fetch-site", "same-origin")
        .header("hx-request", "true");
    if let Some(token) = session {
        req = req.header(header::COOKIE, format!("berth_session={token}"));
    }
    req.body(Body::from(body)).unwrap()
}

fn json(uri: &str, bearer: Option<&str>, body: &serde_json::Value) -> Request<Body> {
    let mut req = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    req.body(Body::from(body.to_string())).unwrap()
}

fn bearer_get(uri: &str, token: &str) -> Request<Body> {
    Request::get(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    /// `code` of a problem+json reply.
    fn problem(&self) -> String {
        assert_eq!(
            self.headers[header::CONTENT_TYPE],
            "application/problem+json",
            "{}",
            self.body
        );
        self.json()["code"].as_str().unwrap().to_owned()
    }
}

/// Text between `start` and the next `end`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text
        .find(start)
        .unwrap_or_else(|| panic!("{start} not in {text}"))
        + start.len();
    let len = text[from..].find(end).unwrap();
    &text[from..from + len]
}

async fn set_up(state: &AppState) -> String {
    let token = state.setup.get().expect("setup pending");
    let reply = send(
        state,
        post(
            "/setup",
            None,
            &[
                ("token", &token),
                ("name", "Family shelf"),
                ("password", PASSWORD),
                ("confirm", PASSWORD),
            ],
        ),
    )
    .await;
    assert_eq!(reply.location(), Some("/"), "{}", reply.body);
    reply.session().expect("session cookie")
}

#[tokio::test]
async fn setup_requires_the_token_and_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;

    assert_eq!(
        send(&state, get("/setup", None)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&state, get("/setup?token=guess", None)).await.status,
        StatusCode::NOT_FOUND
    );
    let token = state.setup.get().unwrap();
    let page = send(&state, get(&format!("/setup?token={token}"), None)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("name=\"password\""));

    let short = send(
        &state,
        post(
            "/setup",
            None,
            &[
                ("token", &token),
                ("name", "X"),
                ("password", "short"),
                ("confirm", "short"),
            ],
        ),
    )
    .await;
    assert_eq!(short.status, StatusCode::OK);
    assert!(short.body.contains("role=\"alert\""));
    assert!(short.session().is_none());

    let session = set_up(&state).await;
    assert!(state.setup.get().is_none());
    assert_eq!(
        send(&state, get("/", Some(&session))).await.status,
        StatusCode::OK
    );

    let again = send(&state, get(&format!("/setup?token={token}"), None)).await;
    assert_eq!(again.location(), Some("/login"));
}

#[tokio::test]
async fn pages_need_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let reply = send(&state, get("/", None)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), Some("/login"));
    let reply = send(&state, get("/settings", Some("forged"))).await;
    assert_eq!(reply.location(), Some("/login"));
    let login = send(&state, get("/login", None)).await;
    // Not set up yet: the login page explains where the setup link is instead of a form.
    assert_eq!(login.status, StatusCode::OK);
    assert!(!login.body.contains("name=\"password\""));
}

#[tokio::test]
async fn login_and_logout() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    set_up(&state).await;

    let wrong = send(
        &state,
        post("/login", None, &[("password", "not the password")]),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::OK);
    assert!(wrong.body.contains("role=\"alert\""));
    assert!(wrong.session().is_none());

    let ok = send(&state, post("/login", None, &[("password", PASSWORD)])).await;
    assert_eq!(ok.location(), Some("/"));
    let session = ok.session().unwrap();
    let cookie = ok.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
    assert_eq!(
        send(&state, get("/settings", Some(&session))).await.status,
        StatusCode::OK
    );
    let password_tab = send(&state, get("/settings/password", Some(&session))).await;
    assert!(password_tab.body.contains("name=\"current\""));

    let out = send(&state, post("/logout", Some(&session), &[])).await;
    assert_eq!(out.location(), Some("/login"));
    assert_eq!(
        send(&state, get("/", Some(&session))).await.location(),
        Some("/login")
    );
    let web_devices: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE platform = 'web'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    // The setup session remains; the logged-out browser is gone.
    assert_eq!(web_devices, 1);
}

#[tokio::test]
async fn cross_origin_posts_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;

    let mut req = post("/settings", Some(&session), &[("name", "Pwned")]);
    req.headers_mut()
        .insert("sec-fetch-site", "cross-site".parse().unwrap());
    assert_eq!(send(&state, req).await.status, StatusCode::FORBIDDEN);

    let req = Request::post("/logout")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::HOST, "localhost:8080")
        .header(header::COOKIE, format!("berth_session={session}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&state, req).await.status, StatusCode::FORBIDDEN);
    assert_eq!(state.config.load().name, "Family shelf");
}

#[tokio::test]
async fn settings_apply_to_server_info() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;

    let bad = send(
        &state,
        post(
            "/settings",
            Some(&session),
            &[
                ("name", "Home"),
                ("lan_url", "192.168.0.4"),
                ("wan_url", ""),
            ],
        ),
    )
    .await;
    assert!(bad.body.contains("role=\"alert\""), "{}", bad.body);
    assert_eq!(state.config.load().name, "Family shelf");

    let saved = send(
        &state,
        post(
            "/settings",
            Some(&session),
            &[
                ("name", "Home books"),
                ("lan_url", "http://192.168.0.4:8080"),
                ("wan_url", ""),
            ],
        ),
    )
    .await;
    assert!(saved.body.contains("role=\"status\""), "{}", saved.body);
    let config = state.config.load();
    assert_eq!(
        config.lan_url.as_ref().unwrap().as_str(),
        "http://192.168.0.4:8080/"
    );
    assert!(config.wan_url.is_none());

    let info = send(&state, get("/v1/server-info", None)).await;
    let info: serde_json::Value = serde_json::from_str(&info.body).unwrap();
    assert_eq!(info["name"], "Home books");
}

#[tokio::test]
async fn password_change_and_console_reset() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let other = send(&state, post("/login", None, &[("password", PASSWORD)]))
        .await
        .session()
        .unwrap();

    let new = "brand new password";
    let wrong = send(
        &state,
        post(
            "/settings/password",
            Some(&session),
            &[("current", "nope"), ("password", new), ("confirm", new)],
        ),
    )
    .await;
    assert!(wrong.body.contains("role=\"alert\""));
    let changed = send(
        &state,
        post(
            "/settings/password",
            Some(&session),
            &[("current", PASSWORD), ("password", new), ("confirm", new)],
        ),
    )
    .await;
    assert!(changed.body.contains("role=\"status\""), "{}", changed.body);
    // The current browser stays signed in, the other one is signed out.
    assert_eq!(
        send(&state, get("/", Some(&session))).await.status,
        StatusCode::OK
    );
    assert_eq!(
        send(&state, get("/", Some(&other))).await.location(),
        Some("/login")
    );

    auth::reset_password(&state.db, "console password", "console password")
        .await
        .unwrap();
    assert_eq!(
        send(&state, get("/", Some(&session))).await.location(),
        Some("/login")
    );
    assert!(
        auth::login(&state.db, "console password", "test")
            .await
            .is_ok()
    );
    assert!(auth::login(&state.db, new, "test").await.is_err());
}

#[tokio::test]
async fn static_assets_are_served() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let css = send(&state, get("/static/admin.css", None)).await;
    assert_eq!(css.status, StatusCode::OK);
    assert!(
        css.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    assert_eq!(
        send(&state, get("/static/nope.js", None)).await.status,
        StatusCode::NOT_FOUND
    );
}

/// Admin adds a listener, shows a code; the phone pairs, refreshes, and gets revoked.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one end-to-end scenario")]
async fn pairing_flow_from_connect_to_revoke() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;

    // Adding a listener opens the QR panel under their row right away.
    let page = send(&state, post("/users", Some(&session), &[("name", "Anna")])).await;
    assert_eq!(page.status, StatusCode::OK);
    let status_url = format!("/users/{}", between(&page.body, "hx-get=\"/users/", "\""));
    let user_id = between(&status_url, "/users/", "/pairing/").to_owned();
    assert!(page.body.contains(&format!("id=\"pairing-{user_id}\"")));
    let code = format!(
        "{}{}",
        between(&page.body, "select-all\">", "<"),
        between(&page.body, "</span><span class=\"select-all\">", "<")
    );
    assert_eq!(code.len(), 8, "{code}");
    // No LAN/WAN set: the QR points at the address the page was opened at.
    assert!(page.body.contains("<svg"));
    assert_eq!(
        send(&state, get(&status_url, Some(&session))).await.status,
        StatusCode::NO_CONTENT
    );

    // Typed by hand: lower case and a separator are fine.
    let typed = format!("{}-{}", &code[..4], code[4..].to_lowercase());
    let paired = send(
        &state,
        json(
            "/v1/auth/pair",
            None,
            &serde_json::json!({ "pairing_token": typed, "device_name": "Pixel 9", "platform": "android" }),
        ),
    )
    .await;
    assert_eq!(paired.status, StatusCode::OK, "{}", paired.body);
    let tokens = paired.json();
    assert_eq!(tokens["access_expires_in"], 900);
    let access = tokens["access_token"].as_str().unwrap().to_owned();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_owned();
    let device_id = tokens["device_id"].as_str().unwrap().to_owned();

    let again = send(
        &state,
        json(
            "/v1/auth/pair",
            None,
            &serde_json::json!({ "pairing_token": code, "device_name": "Thief", "platform": "android" }),
        ),
    )
    .await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
    assert_eq!(again.problem(), "pairing_token_used");

    let done = send(&state, get(&status_url, Some(&session))).await;
    assert!(done.body.contains("Pixel 9"), "{}", done.body);

    let me = send(&state, bearer_get("/v1/me", &access)).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.json()["user_id"], user_id);
    assert_eq!(me.json()["user_name"], "Anna");
    assert_eq!(me.json()["scopes"], serde_json::json!(["listen"]));

    // A listening device is not an admin.
    let forbidden = send(
        &state,
        json(
            "/v1/admin/pairing-tokens",
            Some(&access),
            &serde_json::json!({ "user_id": user_id }),
        ),
    )
    .await;
    assert_eq!(forbidden.problem(), "forbidden");

    let rotated = send(
        &state,
        json(
            "/v1/auth/refresh",
            None,
            &serde_json::json!({ "refresh_token": refresh }),
        ),
    )
    .await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    let refresh = rotated.json()["refresh_token"].as_str().unwrap().to_owned();

    // The phone counts for Anna; the admin's browser does not count for the admin.
    let users = send(&state, get("/users", Some(&session))).await;
    assert!(users.body.contains("Devices: 1"), "{}", users.body);
    assert!(users.body.contains("Devices: 0"), "{}", users.body);

    let devices = send(&state, get("/devices", Some(&session))).await;
    assert!(devices.body.contains("Pixel 9"));
    assert!(
        devices
            .body
            .contains(&format!("/devices/{device_id}/revoke"))
    );
    let revoked = send(
        &state,
        post(&format!("/devices/{device_id}/revoke"), Some(&session), &[]),
    )
    .await;
    // The row goes away; only active devices are listed.
    assert_eq!(revoked.status, StatusCode::OK);
    assert!(revoked.body.trim().is_empty(), "{}", revoked.body);
    let devices = send(&state, get("/devices", Some(&session))).await;
    assert!(!devices.body.contains("Pixel 9"), "{}", devices.body);
    // The admin's own browser is a session, not a device: not listed, not revocable here.
    let browser: String = sqlx::query_scalar("SELECT id FROM devices WHERE platform = 'web'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(!devices.body.contains(&browser));
    let own = send(
        &state,
        post(&format!("/devices/{browser}/revoke"), Some(&session), &[]),
    )
    .await;
    assert_eq!(own.status, StatusCode::NOT_FOUND);

    let me = send(&state, bearer_get("/v1/me", &access)).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
    assert_eq!(me.problem(), "device_revoked");
    let refreshed = send(
        &state,
        json(
            "/v1/auth/refresh",
            None,
            &serde_json::json!({ "refresh_token": refresh }),
        ),
    )
    .await;
    assert_eq!(refreshed.problem(), "device_revoked");
}

#[tokio::test]
async fn pairing_errors_are_problems() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    set_up(&state).await;
    let owner: String = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let pair = |code: &str| {
        json(
            "/v1/auth/pair",
            None,
            &serde_json::json!({ "pairing_token": code, "device_name": "Phone", "platform": "ios" }),
        )
    };

    assert_eq!(
        send(&state, pair("ZZZZZZZZ")).await.problem(),
        "pairing_token_invalid"
    );
    assert_eq!(
        send(&state, pair("short")).await.problem(),
        "pairing_token_invalid"
    );

    // Issued three minutes ago: past its two minutes.
    let old = pairing::create(&state.db, &tenant(&state).await, &owner, now() - 180_000)
        .await
        .unwrap();
    assert_eq!(
        send(&state, pair(&old.code)).await.problem(),
        "pairing_token_expired"
    );

    let bad = send(
        &state,
        Request::post("/v1/auth/pair")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"pairing_token": 1}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.problem(), "validation");
    assert_eq!(bad.json()["detail"], "invalid_body");

    assert_eq!(
        send(&state, get("/v1/me", None)).await.problem(),
        "unauthenticated"
    );
    assert_eq!(
        send(&state, bearer_get("/v1/me", "not.a.jwt"))
            .await
            .problem(),
        "unauthenticated"
    );
}

#[tokio::test]
async fn pairing_is_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    // One router for all requests, as in `berth serve`: the limiter lives in it.
    let app = berth_server::router(state);
    let mut statuses = Vec::new();
    let mut last = None;
    for _ in 0..12 {
        let mut req = json(
            "/v1/auth/pair",
            None,
            &serde_json::json!({ "pairing_token": "ZZZZZZZZ", "device_name": "x", "platform": "ios" }),
        );
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50_000))));
        let response = app.clone().oneshot(req).await.unwrap();
        statuses.push(response.status());
        last = Some(response);
    }
    assert!(
        statuses[..10]
            .iter()
            .all(|s| *s == StatusCode::UNAUTHORIZED)
    );
    let last = last.unwrap();
    assert_eq!(last.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        last.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );
    let body = last.into_body().collect().await.unwrap().to_bytes();
    let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(problem["code"], "rate_limited");
}

#[tokio::test]
async fn cors_allows_only_known_origins() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let preflight = |origin: &str| {
        Request::options("/v1/auth/pair")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
            .body(Body::empty())
            .unwrap()
    };
    // Debug builds (tests) accept a local web build of the app.
    let local = send(&state, preflight("http://localhost:5173")).await;
    assert_eq!(
        local.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "http://localhost:5173"
    );
    let evil = send(&state, preflight("https://evil.example")).await;
    assert!(
        !evil
            .headers
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
    // The admin pages never answer cross-origin.
    let admin = send(
        &state,
        Request::get("/login")
            .header(header::ORIGIN, "http://localhost:5173")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(
        !admin
            .headers
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
}

#[tokio::test]
async fn users_page_adds_listeners_and_opens_qr() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;

    // A rejected name reopens the popup with the error.
    let empty = send(&state, post("/users", Some(&session), &[("name", " ")])).await;
    assert!(empty.body.contains("role=\"alert\""));
    assert!(empty.body.contains("showModal()"));
    assert!(!empty.body.contains("/pairing/"));

    let owner: String = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(&state.db)
        .await
        .unwrap();
    // The row button: a QR panel for that user only.
    let panel = send(
        &state,
        post(&format!("/users/{owner}/connect"), Some(&session), &[]),
    )
    .await;
    assert!(panel.body.contains(&format!("/users/{owner}/pairing/")));
    assert!(!panel.body.contains("<html"));
    // Without JavaScript the link opens the page with the panel.
    let page = send(
        &state,
        get(&format!("/users?connect={owner}"), Some(&session)),
    )
    .await;
    assert!(page.body.contains(&format!("/users/{owner}/pairing/")));
    // The old page is gone.
    assert_eq!(
        send(&state, get("/connect", Some(&session))).await.status,
        StatusCode::NOT_FOUND
    );
}

async fn tenant(state: &AppState) -> String {
    sqlx::query_scalar("SELECT id FROM tenants")
        .fetch_one(&state.db)
        .await
        .unwrap()
}

fn now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

// ── Storage ──────────────────────────────────────────────

/// Nothing listens on port 1: an S3 endpoint that fails the check fast.
const DEAD_ENDPOINT: &str = "http://127.0.0.1:1";

fn api(method: &str, uri: &str, token: &str, body: Option<&serde_json::Value>) -> Request<Body> {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json");
    req.body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn storage_id(state: &AppState, name: &str) -> String {
    sqlx::query_scalar("SELECT id FROM storage_profiles WHERE name = $1 AND deleted_at IS NULL")
        .bind(name)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

async fn default_storage(state: &AppState) -> String {
    let value: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'default_storage_id'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    serde_json::from_str(&value).unwrap()
}

/// A phone of the owner with the `admin` scope granted, and its access token.
async fn admin_token(state: &AppState) -> String {
    let tenant_id = tenant(state).await;
    let owner: String = sqlx::query_scalar("SELECT id FROM users WHERE role = 'owner'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let code = pairing::create(&state.db, &tenant_id, &owner, now())
        .await
        .unwrap();
    let paired = send(
        state,
        json(
            "/v1/auth/pair",
            None,
            &serde_json::json!({ "pairing_token": code.code, "device_name": "Tablet", "platform": "android" }),
        ),
    )
    .await;
    let tokens = paired.json();
    let token = tokens["access_token"].as_str().unwrap().to_owned();
    // Listen-only until the scope is granted.
    let forbidden = send(state, bearer_get("/v1/admin/storage-profiles", &token)).await;
    assert_eq!(forbidden.problem(), "forbidden");
    let device: String = sqlx::query_scalar("SELECT id FROM devices WHERE name = 'Tablet'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO device_scopes (tenant_id, device_id, scope) VALUES ($1, $2, 'admin')")
        .bind(&tenant_id)
        .bind(&device)
        .execute(&state.db)
        .await
        .unwrap();
    token
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one admin journey: add, edit, check, default, delete"
)]
async fn storage_page_manages_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let data_dir = dir.path().join("data");

    // Setup made a checked local default in <data_dir>/blobs.
    let local = storage_id(&state, "Local").await;
    assert_eq!(default_storage(&state).await, local);
    assert!(data_dir.join("blobs").is_dir());
    let page = send(&state, get("/storage", Some(&session))).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Passed"), "{}", page.body);
    assert!(page.body.contains(&format!("/storage/{local}/verify")));
    assert!(
        !page.body.contains(&format!("/storage/{local}/delete")),
        "the default cannot be deleted"
    );

    // A rejected form reopens the add dialog with the error.
    let empty = send(
        &state,
        post(
            "/storage",
            Some(&session),
            &[("kind", "local"), ("name", " "), ("path", "x")],
        ),
    )
    .await;
    assert!(empty.body.contains("role=\"alert\""), "{}", empty.body);
    assert!(empty.body.contains("x-init=\"$el.showModal()\""));
    assert!(!empty.body.contains("<html"));

    // A new folder is created and checked right away.
    let added = send(
        &state,
        post(
            "/storage",
            Some(&session),
            &[("kind", "local"), ("name", "Archive"), ("path", "archive")],
        ),
    )
    .await;
    assert!(
        !added.body.contains("x-init=\"$el.showModal()\""),
        "{}",
        added.body
    );
    assert!(data_dir.join("archive").is_dir());
    let archive = storage_id(&state, "Archive").await;
    let duplicate = send(
        &state,
        post(
            "/storage",
            Some(&session),
            &[("kind", "local"), ("name", "Archive"), ("path", "other")],
        ),
    )
    .await;
    assert!(
        duplicate.body.contains("already exists"),
        "{}",
        duplicate.body
    );

    // S3 keys are stored encrypted, never shown, and kept when the edit leaves them empty.
    let s3 = send(
        &state,
        post(
            "/storage",
            Some(&session),
            &[
                ("kind", "s3"),
                ("name", "Bucket"),
                ("endpoint", DEAD_ENDPOINT),
                ("bucket", "books"),
                ("access_key_id", "AKIDEXAMPLE"),
                ("secret_access_key", "very-secret-key"),
            ],
        ),
    )
    .await;
    assert!(s3.body.contains("does not respond"), "{}", s3.body);
    assert!(!s3.body.contains("very-secret-key") && !s3.body.contains("AKIDEXAMPLE"));
    let bucket = storage_id(&state, "Bucket").await;
    let stored = || async {
        sqlx::query_as::<_, (String, String, Option<i64>, Option<String>)>(
            "SELECT config, secrets_encrypted, verified_at, verify_error FROM storage_profiles WHERE id = $1",
        )
        .bind(&bucket)
        .fetch_one(&state.db)
        .await
        .unwrap()
    };
    let (config, secrets, _, error) = stored().await;
    assert!(!secrets.contains("very-secret") && !config.contains("very-secret"));
    assert_eq!(error.as_deref(), Some("storage_unreachable"));
    let renamed = send(
        &state,
        post(
            &format!("/storage/{bucket}"),
            Some(&session),
            &[
                ("kind", "s3"),
                ("name", "Bucket 2"),
                ("endpoint", DEAD_ENDPOINT),
                ("bucket", "books"),
            ],
        ),
    )
    .await;
    assert!(renamed.body.contains("Bucket 2"), "{}", renamed.body);
    let (_, kept, _, error) = stored().await;
    assert_eq!(kept, secrets, "empty keys keep the saved ones");
    assert_eq!(
        error.as_deref(),
        Some("storage_unreachable"),
        "same place, same result"
    );
    // A new location clears the old check.
    send(
        &state,
        post(
            &format!("/storage/{bucket}"),
            Some(&session),
            &[
                ("kind", "s3"),
                ("name", "Bucket 2"),
                ("endpoint", DEAD_ENDPOINT),
                ("bucket", "audio"),
            ],
        ),
    )
    .await;
    assert_eq!(stored().await.3, None);

    // The default moves; the old default can then be deleted, the new one cannot.
    let verified = send(
        &state,
        post(&format!("/storage/{archive}/verify"), Some(&session), &[]),
    )
    .await;
    assert!(verified.body.contains("Passed"));
    send(
        &state,
        post(&format!("/storage/{archive}/default"), Some(&session), &[]),
    )
    .await;
    assert_eq!(default_storage(&state).await, archive);
    let deleted = send(
        &state,
        post(&format!("/storage/{local}/delete"), Some(&session), &[]),
    )
    .await;
    assert!(!deleted.body.contains(&format!("storage-{local}")));
    let refused = send(
        &state,
        post(&format!("/storage/{archive}/delete"), Some(&session), &[]),
    )
    .await;
    assert!(
        refused.body.contains("cannot be deleted"),
        "{}",
        refused.body
    );
    assert_eq!(storage_id(&state, "Archive").await, archive);
}

#[tokio::test]
async fn setup_checks_s3_storage_first() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let token = state.setup.get().unwrap();
    let reply = send(
        &state,
        post(
            "/setup",
            None,
            &[
                ("token", &token),
                ("name", "Family shelf"),
                ("password", PASSWORD),
                ("confirm", PASSWORD),
                ("storage_kind", "s3"),
                ("storage_endpoint", DEAD_ENDPOINT),
                ("storage_bucket", "books"),
                ("storage_access_key_id", "AKIDEXAMPLE"),
                ("storage_secret_access_key", "very-secret-key"),
            ],
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("does not respond"), "{}", reply.body);
    assert!(!reply.body.contains("very-secret-key"));
    assert!(
        reply.body.contains("value=\"books\""),
        "the form keeps the bucket"
    );
    let tenants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenants")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(tenants, 0, "nothing is created");
    assert!(state.setup.get().is_some(), "the token still works");
}

#[tokio::test]
async fn storage_api_crud() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    set_up(&state).await;
    let token = admin_token(&state).await;

    let list = send(&state, bearer_get("/v1/admin/storage-profiles", &token)).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    let profiles = list.json();
    assert_eq!(profiles[0]["kind"], "local");
    assert_eq!(profiles[0]["path"], "blobs");
    assert_eq!(profiles[0]["is_default"], true);
    assert!(profiles[0].get("bucket").is_none());

    let created = send(
        &state,
        api(
            "POST",
            "/v1/admin/storage-profiles",
            &token,
            Some(&serde_json::json!({
                "name": "Bucket", "kind": "s3", "endpoint": DEAD_ENDPOINT, "bucket": "books",
                "access_key_id": "AKIDEXAMPLE", "secret_access_key": "very-secret-key",
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert!(!created.body.contains("very-secret-key") && !created.body.contains("AKIDEXAMPLE"));
    let profile = created.json();
    let id = profile["id"].as_str().unwrap().to_owned();
    assert_eq!(profile["delivery_mode"], "presigned");
    assert_eq!(profile["path_style"], true);
    assert_eq!(profile["verified_at"], serde_json::Value::Null);

    let no_keys = send(
        &state,
        api(
            "POST",
            "/v1/admin/storage-profiles",
            &token,
            Some(&serde_json::json!({ "name": "Other", "kind": "s3", "bucket": "b" })),
        ),
    )
    .await;
    assert_eq!(no_keys.problem(), "validation");
    assert_eq!(no_keys.json()["detail"], "storage_keys_required");

    let item = format!("/v1/admin/storage-profiles/{id}");
    let updated = send(
        &state,
        api(
            "PUT",
            &item,
            &token,
            Some(&serde_json::json!({
                "name": "Books", "kind": "s3", "endpoint": DEAD_ENDPOINT, "bucket": "books",
                "delivery_mode": "proxy",
            })),
        ),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.json()["name"], "Books");
    assert_eq!(updated.json()["delivery_mode"], "proxy");
    let kind = send(
        &state,
        api(
            "PUT",
            &item,
            &token,
            Some(&serde_json::json!({ "name": "Books", "kind": "local", "path": "x" })),
        ),
    )
    .await;
    assert_eq!(kind.json()["detail"], "storage_kind_invalid");

    let checked = send(&state, api("POST", &format!("{item}/verify"), &token, None)).await;
    assert_eq!(checked.status, StatusCode::OK);
    assert_eq!(checked.json()["verify_error"], "storage_unreachable");

    let default = send(
        &state,
        api("POST", &format!("{item}/default"), &token, None),
    )
    .await;
    assert_eq!(default.json()["is_default"], true);
    let refused = send(&state, api("DELETE", &item, &token, None)).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json()["detail"], "storage_is_default");

    let local = storage_id(&state, "Local").await;
    send(
        &state,
        api(
            "POST",
            &format!("/v1/admin/storage-profiles/{local}/default"),
            &token,
            None,
        ),
    )
    .await;
    let deleted = send(&state, api("DELETE", &item, &token, None)).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        send(&state, bearer_get(&item, &token)).await.problem(),
        "not_found"
    );
}

// ── Upload and sync ──────────────────────────────────────

fn put_bytes(uri: &str, token: &str, bytes: Vec<u8>) -> Request<Body> {
    Request::put(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(bytes))
        .unwrap()
}

fn chapter_mp3(title: &str, track: u32, seconds: u32) -> Vec<u8> {
    fixtures::mp3(&fixtures::Mp3 {
        seconds,
        title: Some(title),
        album: Some("Сами боги"),
        artist: Some("Айзек Азимов"),
        narrator: None,
        track: Some(track),
        chapters: &[],
        cover: false,
    })
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one journey: upload, analyse, save, sync, cover, delete"
)]
async fn upload_analyse_save_and_sync() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    set_up(&state).await;
    let token = admin_token(&state).await;

    // A folder: two chapters out of order by name, and a cover next to them.
    let second = chapter_mp3("Вторая", 2, 2);
    let first = chapter_mp3("Первая", 1, 1);
    let created = send(
        &state,
        api(
            "POST",
            "/v1/ingest/uploads",
            &token,
            Some(&serde_json::json!({
                "name": "Азимов Айзек - Сами боги [Юрий Красиков]",
                "files": [
                    { "name": "b.mp3", "size": second.len() },
                    { "name": "a.mp3", "size": first.len() },
                    { "name": "cover.png", "size": fixtures::PNG.len() },
                ],
            })),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let created = created.json();
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(
        created["targets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["method"] == "server")
    );

    let early = send(
        &state,
        api(
            "POST",
            &format!("/v1/ingest/uploads/{id}/complete"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(early.json()["detail"], "upload_file_missing");
    let short = send(
        &state,
        put_bytes(
            &format!("/v1/ingest/uploads/{id}/files/0"),
            &token,
            vec![1, 2, 3],
        ),
    )
    .await;
    assert_eq!(short.json()["detail"], "upload_size_mismatch");
    for (idx, bytes) in [second.clone(), first, fixtures::PNG.to_vec()]
        .into_iter()
        .enumerate()
    {
        let stored = send(
            &state,
            put_bytes(
                &format!("/v1/ingest/uploads/{id}/files/{idx}"),
                &token,
                bytes,
            ),
        )
        .await;
        assert_eq!(stored.status, StatusCode::NO_CONTENT, "{}", stored.body);
    }
    let analysing = send(
        &state,
        api(
            "POST",
            &format!("/v1/ingest/uploads/{id}/complete"),
            &token,
            None,
        ),
    )
    .await;
    assert_eq!(analysing.status, StatusCode::ACCEPTED, "{}", analysing.body);
    assert_eq!(analysing.json()["status"], "analyzing");
    assert_eq!(berth_server::jobs::run_due(&state).await.unwrap(), 1);

    let ready = send(
        &state,
        bearer_get(&format!("/v1/ingest/uploads/{id}"), &token),
    )
    .await
    .json();
    assert_eq!(ready["status"], "ready", "{ready}");
    let draft = &ready["draft"];
    assert_eq!(draft["title"], "Сами боги");
    assert_eq!(draft["authors"], serde_json::json!(["Айзек Азимов"]));
    assert_eq!(
        draft["narrators"],
        serde_json::json!(["Юрий Красиков"]),
        "from the folder name"
    );
    assert_eq!(draft["files"][0]["name"], "a.mp3", "track order");
    assert_eq!(draft["chapters"][1]["title"], "Вторая");
    assert!(draft["cover_hash"].is_string());

    let saved = send(
        &state,
        api(
            "POST",
            &format!("/v1/ingest/uploads/{id}/save"),
            &token,
            Some(&serde_json::json!({
                "title": "Сами боги", "authors": ["Айзек Азимов"], "narrators": ["Юрий Красиков"],
                "series": "", "rating": 16,
            })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.body);
    let book_id = saved.json()["book_id"].as_str().unwrap().to_owned();
    let again = send(
        &state,
        api(
            "POST",
            &format!("/v1/ingest/uploads/{id}/save"),
            &token,
            Some(&serde_json::json!({ "title": "x" })),
        ),
    )
    .await;
    assert_eq!(again.json()["detail"], "upload_not_ready");

    // The phone gets the whole book in one page, then nothing new.
    let page = send(&state, bearer_get("/v1/sync", &token)).await.json();
    assert_eq!(page["reset"], true);
    assert_eq!(page["has_more"], false);
    let book = &page["books"][0];
    assert_eq!(book["id"], book_id.as_str());
    assert_eq!(book["rating"], 16);
    assert_eq!(book["files"].as_array().unwrap().len(), 2);
    assert_eq!(
        book["files"][1]["offset_ms"],
        book["files"][0]["duration_ms"]
    );
    assert_eq!(book["chapters"].as_array().unwrap().len(), 2);
    assert_eq!(book["duration_ms"], book["chapters"][1]["end_ms"]);
    let cursor = page["cursor"].as_str().unwrap().to_owned();
    let quiet = send(
        &state,
        bearer_get(&format!("/v1/sync?cursor={cursor}"), &token),
    )
    .await
    .json();
    assert_eq!(
        (
            quiet["reset"].clone(),
            quiet["books"].as_array().unwrap().len()
        ),
        (serde_json::json!(false), 0)
    );

    // The cover is served with caching headers.
    let cover = send(
        &state,
        bearer_get(&format!("/v1/books/{book_id}/cover"), &token),
    )
    .await;
    assert_eq!(cover.status, StatusCode::OK);
    assert_eq!(cover.headers[header::CONTENT_TYPE], "image/png");
    let etag = cover.headers[header::ETAG].to_str().unwrap().to_owned();
    let cached = Request::get(format!("/v1/books/{book_id}/cover"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::IF_NONE_MATCH, etag)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&state, cached).await.status, StatusCode::NOT_MODIFIED);

    // The same bytes again need no transfer.
    let digest = hex::encode(sha2::Sha256::digest(&second));
    let dedup = send(
        &state,
        api(
            "POST",
            "/v1/ingest/uploads",
            &token,
            Some(&serde_json::json!({ "name": "again", "files": [{ "name": "x.mp3", "size": second.len(), "sha256": digest }] })),
        ),
    )
    .await
    .json();
    assert_eq!(dedup["targets"][0]["method"], "stored");

    // A deleted book is removed on the next sync.
    let tenant_id = tenant(&state).await;
    berth_server::service::books::delete(&state.db, &tenant_id, &book_id)
        .await
        .unwrap();
    let gone = send(
        &state,
        bearer_get(&format!("/v1/sync?cursor={cursor}"), &token),
    )
    .await
    .json();
    assert_eq!(gone["removed"], serde_json::json!([book_id]));
    assert_eq!(
        send(
            &state,
            bearer_get(&format!("/v1/books/{book_id}/cover"), &token)
        )
        .await
        .problem(),
        "not_found"
    );
}

/// A same-origin browser request with the session cookie.
fn browser(
    method: &str,
    uri: &str,
    session: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("berth_session={session}"))
        .header("sec-fetch-site", "same-origin")
        .header("hx-request", "true")
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one admin journey: upload page, library, book page"
)]
async fn admin_uploads_and_edits_books() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;

    let empty = send(&state, get("/", Some(&session))).await;
    assert!(empty.body.contains("href=\"/upload\""), "{}", empty.body);
    let page = send(&state, get("/upload", Some(&session))).await;
    assert!(
        page.body.contains("id=\"dropzone\"") && page.body.contains("data-presigned=\"false\"")
    );

    // The browser script: targets, files, completion, then the card polls.
    let m4b = fixtures::m4b(&fixtures::M4b {
        seconds: 90,
        title: "Основание",
        artist: "Айзек Азимов",
        chapters: &[(0, "Психоисторики"), (30_000, "Энциклопедисты")],
    });
    let new = serde_json::json!({ "name": "Основание.m4b", "files": [{ "name": "Основание.m4b", "size": m4b.len() }] });
    let created = send(
        &state,
        browser(
            "POST",
            "/upload/new",
            &session,
            "application/json",
            new.to_string().into_bytes(),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let id = created.json()["id"].as_str().unwrap().to_owned();
    let card = send(&state, get(&format!("/upload/{id}"), Some(&session))).await;
    assert!(card.body.contains("data-progress"), "{}", card.body);
    let stored = send(
        &state,
        browser(
            "PUT",
            &format!("/upload/{id}/files/0"),
            &session,
            "application/octet-stream",
            m4b,
        ),
    )
    .await;
    assert_eq!(stored.status, StatusCode::NO_CONTENT, "{}", stored.body);
    let analysing = send(
        &state,
        post(&format!("/upload/{id}/complete"), Some(&session), &[]),
    )
    .await;
    assert!(
        analysing.body.contains("hx-trigger=\"load delay:2s\""),
        "{}",
        analysing.body
    );
    berth_server::jobs::run_due(&state).await.unwrap();
    let ready = send(&state, get(&format!("/upload/{id}"), Some(&session))).await;
    assert!(ready.body.contains("value=\"Основание\""), "{}", ready.body);
    assert!(ready.body.contains("Энциклопедисты"));
    assert!(
        !ready.body.contains("hx-trigger=\"load delay:2s\""),
        "no more polling"
    );

    let rejected = send(
        &state,
        post(
            &format!("/upload/{id}/save"),
            Some(&session),
            &[("title", " "), ("authors", "Айзек Азимов")],
        ),
    )
    .await;
    assert!(
        rejected.body.contains("role=\"alert\""),
        "{}",
        rejected.body
    );
    let saved = send(
        &state,
        post(
            &format!("/upload/{id}/save"),
            Some(&session),
            &[
                ("title", "Основание"),
                ("authors", "Айзек Азимов"),
                ("narrators", ""),
                ("series", "Основание"),
                ("series_no", "1"),
                ("rating", "12"),
            ],
        ),
    )
    .await;
    let book_id = between(&saved.body, "href=\"/book/", "\"").to_owned();

    // The library lists it, filters find it, the book page edits it.
    let library = send(&state, get("/library", Some(&session))).await;
    let library_url = library.location().unwrap().to_owned();
    let list = send(&state, get(&library_url, Some(&session))).await;
    assert!(
        list.body.contains(&format!("/book/{book_id}")),
        "{}",
        list.body
    );
    assert!(list.body.contains("Основание #1"));
    let found = send(
        &state,
        get(
            &format!("{library_url}?q=%D0%B0%D0%B7%D0%B8%D0%BC%D0%BE%D0%B2"),
            Some(&session),
        ),
    )
    .await;
    assert!(
        found.body.contains(&format!("/book/{book_id}")),
        "case-insensitive Cyrillic search"
    );
    let none = send(
        &state,
        get(&format!("{library_url}?rating=18"), Some(&session)),
    )
    .await;
    assert!(!none.body.contains(&format!("/book/{book_id}")));

    let book = send(&state, get(&format!("/book/{book_id}"), Some(&session))).await;
    assert!(
        book.body.contains("Психоисторики") && book.body.contains("1:30"),
        "{}",
        book.body
    );
    let edited = send(
        &state,
        post(
            &format!("/book/{book_id}"),
            Some(&session),
            &[
                ("title", "Основание"),
                ("authors", "Азимов, Айзек"),
                ("series_no", "x"),
            ],
        ),
    )
    .await;
    assert!(edited.body.contains("role=\"alert\""), "{}", edited.body);
    let edited = send(
        &state,
        post(
            &format!("/book/{book_id}"),
            Some(&session),
            &[("title", "Основание"), ("rating", "16")],
        ),
    )
    .await;
    assert!(edited.body.contains("role=\"status\""), "{}", edited.body);

    // A new cover, then the book goes.
    let boundary = "berthboundary";
    let mut form = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"cover\"; filename=\"c.png\"\r\nContent-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    form.extend_from_slice(fixtures::PNG);
    form.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let covered = send(
        &state,
        browser(
            "POST",
            &format!("/book/{book_id}/cover"),
            &session,
            &format!("multipart/form-data; boundary={boundary}"),
            form,
        ),
    )
    .await;
    assert!(covered.body.contains("cover?v="), "{}", covered.body);
    let image = send(
        &state,
        get(&format!("/book/{book_id}/cover"), Some(&session)),
    )
    .await;
    assert_eq!(image.headers[header::CONTENT_TYPE], "image/png");
    let dashboard = send(&state, get("/", Some(&session))).await;
    assert!(dashboard.body.contains(">1</dd>"), "one book");

    let deleted = send(
        &state,
        post(&format!("/book/{book_id}/delete"), Some(&session), &[]),
    )
    .await;
    assert_eq!(deleted.location(), Some(library_url.as_str()));
    assert_eq!(
        send(&state, get(&format!("/book/{book_id}"), Some(&session)))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
