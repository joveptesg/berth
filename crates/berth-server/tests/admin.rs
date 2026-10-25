//! Integration tests of the web admin (setup wizard, login, sessions, CSRF, settings, users,
//! connect, devices, storage) and of the device API it drives (pairing, refresh, revoke,
//! storage profiles).

use std::{net::SocketAddr, path::Path};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
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
    let code = state.setup.get().expect("setup pending");
    let reply = send(
        state,
        post(
            "/setup",
            None,
            &[
                ("code", &code),
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
async fn setup_requires_the_code_and_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let code = state.setup.get().unwrap();
    assert_eq!(code.len(), 8);

    // The wizard opens at the server address; the code from the log link is filled in.
    let page = send(&state, get("/setup", None)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("name=\"code\" value=\"\""));
    assert!(page.body.contains("name=\"password\""));
    let linked = send(&state, get(&format!("/setup?code={code}"), None)).await;
    assert!(linked.body.contains(&format!("value=\"{code}\"")));

    let form = |code: &str, password: &str| {
        post(
            "/setup",
            None,
            &[
                ("code", code),
                ("name", "Family shelf"),
                ("password", password),
                ("confirm", password),
            ],
        )
    };
    let wrong = send(&state, form("AAAA-AAAA", PASSWORD)).await;
    assert_eq!(wrong.status, StatusCode::OK);
    assert!(wrong.body.contains("setup code"), "{}", wrong.body);
    assert!(wrong.session().is_none());
    let short = send(&state, form(&code, "short")).await;
    assert!(short.body.contains("role=\"alert\""));
    assert!(short.session().is_none());

    // Typed by hand: lower case with a dash.
    let typed = format!("{}-{}", &code[..4], &code[4..]).to_lowercase();
    let reply = send(&state, form(&typed, PASSWORD)).await;
    assert_eq!(reply.location(), Some("/"), "{}", reply.body);
    let session = reply.session().expect("session cookie");
    assert!(state.setup.get().is_none());
    assert_eq!(
        send(&state, get("/", Some(&session))).await.status,
        StatusCode::OK
    );

    let again = send(&state, get(&format!("/setup?code={code}"), None)).await;
    assert_eq!(again.location(), Some("/login"));
    let late = send(&state, form(&code, PASSWORD)).await;
    assert_eq!(late.location(), Some("/login"));
}

#[tokio::test]
async fn setup_remembers_the_address_it_was_opened_at() {
    for (host, proto, lan, wan) in [
        (
            "berth-test-acae1371.178.18.249.144.sslip.io",
            Some("https"),
            None,
            Some("https://berth-test-acae1371.178.18.249.144.sslip.io/"),
        ),
        (
            "178.18.249.144:8080",
            None,
            None,
            Some("http://178.18.249.144:8080/"),
        ),
        (
            "192.168.1.5:8080",
            None,
            Some("http://192.168.1.5:8080/"),
            None,
        ),
        ("localhost:8080", None, None, None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let state = start(dir.path()).await;
        let code = state.setup.get().unwrap();
        let mut req = post(
            "/setup",
            None,
            &[
                ("code", &code),
                ("name", "Family shelf"),
                ("password", PASSWORD),
                ("confirm", PASSWORD),
            ],
        );
        req.headers_mut()
            .insert(header::HOST, HeaderValue::from_static(host));
        if let Some(proto) = proto {
            req.headers_mut()
                .insert("x-forwarded-proto", HeaderValue::from_static(proto));
        }
        let reply = send(&state, req).await;
        assert_eq!(reply.location(), Some("/"), "{host}: {}", reply.body);
        let config = state.config.load();
        assert_eq!(config.lan_url.as_ref().map(url::Url::as_str), lan, "{host}");
        assert_eq!(config.wan_url.as_ref().map(url::Url::as_str), wan, "{host}");
    }
}

#[tokio::test]
async fn pages_need_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    // Not set up yet: the server address leads to the setup wizard.
    for path in ["/", "/login"] {
        let reply = send(&state, get(path, None)).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(reply.location(), Some("/setup"), "{path}");
    }
    set_up(&state).await;
    let reply = send(&state, get("/", None)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), Some("/login"));
    let reply = send(&state, get("/settings", Some("forged"))).await;
    assert_eq!(reply.location(), Some("/login"));
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
    assert!(
        info.get("server_urls").is_none(),
        "addresses only for devices"
    );

    // Paired phones learn the addresses with the library sync.
    let token = admin_token(&state).await;
    let page = send(&state, bearer_get("/v1/sync", &token)).await.json();
    assert_eq!(
        page["server_urls"],
        serde_json::json!(["http://192.168.0.4:8080"])
    );
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
    let status_url = page
        .body
        .split("hx-get=\"/users/")
        .skip(1)
        .map(|rest| &rest[..rest.find('"').unwrap()])
        .find(|url| url.contains("/pairing/"))
        .map(|url| format!("/users/{url}"))
        .expect("the panel polls the pairing status");
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

/// A phone of the owner (it gets the `admin` scope) and its access token.
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
    // The owner's phone gets the admin scope when it pairs.
    let scopes: Vec<String> = sqlx::query_scalar(
        "SELECT s.scope FROM device_scopes s JOIN devices d ON d.id = s.device_id
         WHERE d.name = 'Tablet' AND d.tenant_id = $1 ORDER BY s.scope",
    )
    .bind(&tenant_id)
    .fetch_all(&state.db)
    .await
    .unwrap();
    assert_eq!(scopes, ["admin", "listen"]);
    tokens["access_token"].as_str().unwrap().to_owned()
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
    let code = state.setup.get().unwrap();
    let reply = send(
        &state,
        post(
            "/setup",
            None,
            &[
                ("code", &code),
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
    assert!(state.setup.get().is_some(), "the code still works");
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one journey: create, update, CDN, check, default, delete"
)]
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
    let cdn = |url: &str| {
        serde_json::json!({
            "name": "Books", "kind": "s3", "endpoint": DEAD_ENDPOINT, "bucket": "books",
            "delivery_mode": "cdn", "cdn_url": url,
        })
    };
    let no_url = send(&state, api("PUT", &item, &token, Some(&cdn("")))).await;
    assert_eq!(no_url.json()["detail"], "storage_cdn_url_required");
    let with_cdn = send(
        &state,
        api("PUT", &item, &token, Some(&cdn("https://cdn.example.com/"))),
    )
    .await;
    assert_eq!(with_cdn.status, StatusCode::OK, "{}", with_cdn.body);
    assert_eq!(with_cdn.json()["delivery_mode"], "cdn");
    assert_eq!(with_cdn.json()["cdn_url"], "https://cdn.example.com");
    assert_eq!(with_cdn.json()["cdn_signing"], "none");
    let bunny = |key: Option<&str>| {
        let mut body = cdn("https://zone.b-cdn.net");
        body["cdn_signing"] = "bunny".into();
        if let Some(key) = key {
            body["cdn_token_key"] = key.into();
        }
        body
    };
    let no_token = send(&state, api("PUT", &item, &token, Some(&bunny(None)))).await;
    assert_eq!(no_token.json()["detail"], "storage_cdn_token_required");
    let signed = send(
        &state,
        api("PUT", &item, &token, Some(&bunny(Some("zone-key")))),
    )
    .await;
    assert_eq!(signed.status, StatusCode::OK, "{}", signed.body);
    assert_eq!(signed.json()["cdn_signing"], "bunny");
    assert!(
        !signed.body.contains("zone-key"),
        "the token key is never returned"
    );
    let kept = send(&state, api("PUT", &item, &token, Some(&bunny(None)))).await;
    assert_eq!(
        kept.status,
        StatusCode::OK,
        "empty keeps the saved token key"
    );
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
        genre: Some("Sci-Fi"),
        language: None,
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
    let session = set_up(&state).await;
    let token = admin_token(&state).await;

    // Sections: the file genre "Sci-Fi" is an alias of the first one.
    for (name, aliases) in [
        ("Фантастика", "Sci-Fi, научная фантастика"),
        ("Детские", ""),
    ] {
        let added = send(
            &state,
            post(
                "/categories",
                Some(&session),
                &[("name", name), ("aliases", aliases)],
            ),
        )
        .await;
        assert_eq!(added.status, StatusCode::OK, "{}", added.body);
        assert!(added.body.contains(name));
    }
    let duplicate = send(
        &state,
        post("/categories", Some(&session), &[("name", "фантастика")]),
    )
    .await;
    assert!(duplicate.body.contains("alert-error"), "{}", duplicate.body);
    let order = |body: &str| body.find("Фантастика").unwrap() < body.find("Детские").unwrap();
    let listed = send(&state, bearer_get("/v1/sync", &token)).await.json();
    let children = listed["categories"][1]["id"].as_str().unwrap().to_owned();
    let moved = send(
        &state,
        post(&format!("/categories/{children}/up"), Some(&session), &[]),
    )
    .await;
    assert!(!order(&moved.body), "Детские moved up");
    let moved = send(
        &state,
        post(&format!("/categories/{children}/down"), Some(&session), &[]),
    )
    .await;
    assert!(order(&moved.body));

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
    assert_eq!(draft["genres"], serde_json::json!(["Sci-Fi"]));
    assert_eq!(draft["language"], "ru", "guessed from the Cyrillic title");
    assert_eq!(draft["category_ids"].as_array().unwrap().len(), 1);

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
    let sections = page["categories"].as_array().unwrap();
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0]["name"], "Фантастика");
    let fantasy = sections[0]["id"].as_str().unwrap().to_owned();
    let children = sections[1]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        book["category_ids"],
        serde_json::json!([fantasy]),
        "matched by alias"
    );

    // The library filters by section.
    let library = book["library_id"].as_str().unwrap().to_owned();
    let listed = |category: &str| {
        send(
            &state,
            get(
                &format!("/library/{library}?category={category}"),
                Some(&session),
            ),
        )
    };
    assert!(listed(&fantasy).await.body.contains("/book/"));
    assert!(
        !listed(&children)
            .await
            .body
            .contains(&format!("/book/{book_id}"))
    );
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

    // A removed section leaves the book, and phones get the book again without it.
    let removed = send(
        &state,
        post(
            &format!("/categories/{fantasy}/delete"),
            Some(&session),
            &[],
        ),
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    let after = send(
        &state,
        bearer_get(&format!("/v1/sync?cursor={cursor}"), &token),
    )
    .await
    .json();
    assert_eq!(after["books"][0]["category_ids"], serde_json::json!([]));
    assert_eq!(after["categories"].as_array().unwrap().len(), 1);

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

    // A file streams by a signed URL without headers, with range requests, and not gzipped.
    let link = send(
        &state,
        bearer_get(&format!("/v1/books/{book_id}/files/1/stream"), &token),
    )
    .await
    .json();
    let url = link["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("/v1/blobs/"), "{url}");
    assert!(link["expires_at"].as_i64().unwrap() > now());
    let whole = send(&state, Request::get(&url).body(Body::empty()).unwrap()).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.headers[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(whole.headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(
        whole.headers[header::CONTENT_LENGTH],
        second.len().to_string().as_str()
    );
    let part = send(
        &state,
        Request::get(&url)
            .header(header::RANGE, "bytes=0-9")
            .header(header::ACCEPT_ENCODING, "gzip")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        part.headers[header::CONTENT_RANGE],
        format!("bytes 0-9/{}", second.len()).as_str()
    );
    assert!(part.headers.get(header::CONTENT_ENCODING).is_none());
    assert_eq!(part.body, String::from_utf8_lossy(&second[..10]));
    let beyond = Request::get(&url)
        .header(header::RANGE, format!("bytes={}-", second.len()))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&state, beyond).await.status,
        StatusCode::RANGE_NOT_SATISFIABLE
    );
    let tampered = format!("{url}x");
    assert_eq!(
        send(&state, Request::get(&tampered).body(Body::empty()).unwrap())
            .await
            .problem(),
        "unauthenticated"
    );
    let redirect = send(
        &state,
        bearer_get(
            &format!("/v1/books/{book_id}/files/0/stream?redirect=1"),
            &token,
        ),
    )
    .await;
    assert_eq!(redirect.status, StatusCode::FOUND);
    assert!(redirect.location().unwrap().starts_with("/v1/blobs/"));
    let missing = send(
        &state,
        bearer_get(&format!("/v1/books/{book_id}/files/9/stream"), &token),
    )
    .await;
    assert_eq!(missing.problem(), "not_found");

    // The download manifest has every file with its URL and hash, and the chapters.
    let manifest = send(
        &state,
        bearer_get(&format!("/v1/books/{book_id}/download-manifest"), &token),
    )
    .await
    .json();
    assert_eq!(manifest["files"].as_array().unwrap().len(), 2);
    assert_eq!(manifest["files"][1]["blob_hash"], digest_of(&second));
    assert!(
        manifest["files"][0]["url"]
            .as_str()
            .unwrap()
            .starts_with("/v1/blobs/")
    );
    assert_eq!(manifest["chapters"].as_array().unwrap().len(), 2);
    assert_eq!(
        manifest["size"].as_i64().unwrap(),
        book["files"][0]["size"].as_i64().unwrap() + book["files"][1]["size"].as_i64().unwrap()
    );

    // A CDN copy of the bytes wins: a public URL under its address, the blob path appended.
    let cdn = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO storage_profiles (id, tenant_id, name, kind, config, delivery_mode, created_at)
         SELECT $1, tenant_id, 'CDN', 's3',
            '{\"endpoint\":\"\",\"bucket\":\"books\",\"region\":\"auto\",\"path_style\":false,
              \"cdn_url\":\"https://cdn.example.com/b\"}',
            'cdn', 0
         FROM storage_profiles WHERE name = 'Local'",
    )
    .bind(&cdn)
    .execute(&state.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO blob_locations (tenant_id, hash, storage_id, state, updated_at)
         SELECT tenant_id, hash, $1, 'active', 0 FROM blob_locations WHERE hash = $2",
    )
    .bind(&cdn)
    .bind(digest_of(&second))
    .execute(&state.db)
    .await
    .unwrap();
    let link = send(
        &state,
        bearer_get(&format!("/v1/books/{book_id}/files/1/stream"), &token),
    )
    .await
    .json();
    let hash = digest_of(&second);
    let public = link["url"].as_str().unwrap();
    assert!(
        public.starts_with("https://cdn.example.com/b/")
            && public.ends_with(&format!(
                "/blobs/sha256/{}/{}/{hash}",
                &hash[..2],
                &hash[2..4]
            )),
        "{public}"
    );
    // A Bunny CDN that signs links: a token valid as long as the link says.
    let tenant_id = tenant(&state).await;
    berth_server::service::storage::update(
        &state,
        &tenant_id,
        &cdn,
        &berth_server::service::storage::ProfileInput {
            name: "CDN".into(),
            kind: "s3".into(),
            bucket: "books".into(),
            region: "auto".into(),
            delivery_mode: "cdn".into(),
            cdn_url: "https://cdn.example.com/b".into(),
            cdn_signing: "bunny".into(),
            cdn_token_key: "zone-key".into(),
            access_key_id: "id".into(),
            secret_access_key: "key".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let link = send(
        &state,
        bearer_get(&format!("/v1/books/{book_id}/files/1/stream"), &token),
    )
    .await
    .json();
    let expires_at = link["expires_at"].as_i64().unwrap();
    let expected = berth_server::storage::bunny_sign(
        public,
        "zone-key",
        u64::try_from(expires_at / 1000).unwrap(),
    )
    .unwrap();
    assert_eq!(link["url"], expected);
    assert!(expires_at > now() + 5 * 3_600_000, "{expires_at}");
    sqlx::query("DELETE FROM blob_locations WHERE storage_id = $1")
        .bind(&cdn)
        .execute(&state.db)
        .await
        .unwrap();

    // Progress: last write wins by the device time corrected for its clock; resends are no-ops.
    let event = |id: &str, at: i64, position: i64| serde_json::json!({ "event_id": id, "book_id": book_id, "position_ms": position, "type": "position", "client_ts": at });
    let ids: Vec<String> = (0..3).map(|_| uuid::Uuid::now_v7().to_string()).collect();
    // This device's clock is an hour behind: its event a minute ago is still the latest.
    let behind = now() - 3_600_000;
    let first_batch = serde_json::json!({ "client_now": behind, "events": [event(&ids[0], behind - 60_000, 5_000)] });
    let positions = send(
        &state,
        api("POST", "/v1/progress/batch", &token, Some(&first_batch)),
    )
    .await
    .json();
    assert_eq!(positions["positions"][0]["position_ms"], 5_000);
    // A late offline event from two hours ago does not overwrite it.
    let late = serde_json::json!({ "client_now": now(), "events": [event(&ids[1], now() - 7_200_000, 1_000)] });
    send(
        &state,
        api("POST", "/v1/progress/batch", &token, Some(&late)),
    )
    .await;
    // Resending the first event with another position changes nothing.
    let resend =
        serde_json::json!({ "client_now": behind, "events": [event(&ids[0], behind, 9_000)] });
    send(
        &state,
        api("POST", "/v1/progress/batch", &token, Some(&resend)),
    )
    .await;
    let current = send(&state, bearer_get("/v1/progress", &token))
        .await
        .json();
    assert_eq!(current["positions"][0]["position_ms"], 5_000);
    assert_eq!(current["positions"][0]["finished"], false);
    let finished = serde_json::json!({ "client_now": now(), "events": [{ "event_id": ids[2], "book_id": book_id, "position_ms": 7_000, "type": "finished", "client_ts": now() }] });
    let done = send(
        &state,
        api("POST", "/v1/progress/batch", &token, Some(&finished)),
    )
    .await
    .json();
    assert_eq!(done["positions"][0]["finished"], true);
    let bad = serde_json::json!({ "client_now": now(), "events": [event("nope", now(), 1)] });
    assert_eq!(
        send(
            &state,
            api("POST", "/v1/progress/batch", &token, Some(&bad))
        )
        .await
        .json()["detail"],
        "progress_event_invalid"
    );

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

    // A revoked device loses its stream URLs at once.
    let device: String = sqlx::query_scalar("SELECT id FROM devices WHERE name = 'Tablet'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    berth_server::service::devices::revoke(&state.db, &tenant_id, &device, now())
        .await
        .unwrap();
    assert_eq!(
        send(&state, Request::get(&url).body(Body::empty()).unwrap())
            .await
            .problem(),
        "device_revoked"
    );
}

fn digest_of(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
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
async fn admin_upload_reports_files_that_did_not_get_through() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let new =
        serde_json::json!({ "name": "Основание", "files": [{ "name": "01.mp3", "size": 10 }] });
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
    let id = created.json()["id"].as_str().unwrap().to_owned();
    let fail = |body: serde_json::Value| {
        browser(
            "POST",
            &format!("/upload/{id}/fail"),
            &session,
            "application/json",
            body.to_string().into_bytes(),
        )
    };

    // A bucket that refuses the browser looks like a network error: its CORS is the suspect.
    let card = send(
        &state,
        fail(serde_json::json!({ "file": 0, "presigned": true, "reason": "network" })),
    )
    .await;
    assert_eq!(card.status, StatusCode::OK, "{}", card.body);
    assert!(card.body.contains("CORS"), "{}", card.body);
    assert!(!card.body.contains("data-progress"), "no progress bar");
    let tenant_id = tenant(&state).await;
    let upload = berth_server::service::ingest::get(&state.db, &tenant_id, &id)
        .await
        .unwrap();
    assert_eq!(upload.status, "failed");
    assert_eq!(upload.error.as_deref(), Some("upload_bucket_unreachable"));

    // Only an upload in progress fails; a second report changes nothing.
    send(
        &state,
        fail(serde_json::json!({ "file": 0, "presigned": false, "reason": "500 boom" })),
    )
    .await;
    let again = send(&state, get(&format!("/upload/{id}"), Some(&session))).await;
    assert!(again.body.contains("CORS"), "{}", again.body);
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
        genre: None,
        language: None,
        track_language: None,
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
    let queue = send(&state, get("/upload", Some(&session))).await;
    assert!(
        queue.body.contains(&format!("{book_id}&#x2f;cover?v=")),
        "the queue shows the book's current cover: {}",
        queue.body
    );
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

#[tokio::test]
async fn database_backups() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let data = dir.path().join("data");
    assert!(
        berth_server::backup::list(&data).is_empty(),
        "a fresh database needs no backup"
    );

    let page = send(&state, get("/settings/backup", Some(&session))).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("/settings/backup/download"));
    assert!(page.body.contains("secret.key"));
    // The server key can be taken away from the backups.
    let key = send(&state, get("/settings/backup/secret-key", Some(&session))).await;
    assert_eq!(
        key.body,
        std::fs::read_to_string(data.join("secret.key"))
            .unwrap()
            .trim()
    );
    let download = send(&state, get("/settings/backup/download", Some(&session))).await;
    assert_eq!(download.status, StatusCode::OK);
    assert!(download.body.starts_with("SQLite format 3"));
    assert!(
        download.headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    assert!(
        std::fs::read_dir(data.join("tmp"))
            .unwrap()
            .next()
            .is_none(),
        "the copy does not stay on disk"
    );
    assert_eq!(
        send(&state, get("/settings/backup/download", None))
            .await
            .location(),
        Some("/login")
    );

    // Before migrations: a copy each time, the newest three kept.
    for _ in 0..4 {
        berth_server::backup::before_migrations(&state.db, &data, true)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let backups = berth_server::backup::list(&data);
    assert_eq!(backups.len(), berth_server::backup::KEEP);
    let newest = &backups[0];
    let page = send(&state, get("/settings/backup", Some(&session))).await;
    assert!(page.body.contains(&newest.name));
    let file = send(
        &state,
        get(&format!("/settings/backup/{}", newest.name), Some(&session)),
    )
    .await;
    assert!(file.body.starts_with("SQLite format 3"));
    let sneaky = send(
        &state,
        get("/settings/backup/..%2Fberth.sqlite", Some(&session)),
    )
    .await;
    assert_eq!(sneaky.status, StatusCode::NOT_FOUND);

    // An automatic backup can be deleted.
    let deleted = send(
        &state,
        post(
            &format!("/settings/backup/{}/delete", newest.name),
            Some(&session),
            &[],
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert!(!deleted.body.contains(&newest.name));
    assert_eq!(
        berth_server::backup::list(&data).len(),
        berth_server::backup::KEEP - 1
    );
}

#[tokio::test]
async fn admin_signs_in_with_the_phone() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    set_up(&state).await;
    let phone = admin_token(&state).await;

    // The sign-in page offers the phone; a code and the browser's claim in a cookie.
    let page = send(&state, get("/login", None)).await;
    assert!(page.body.contains("/login/phone"));
    let started = send(&state, post("/login/phone", None, &[])).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let cookie = started.headers[header::SET_COOKIE].to_str().unwrap();
    let pair = between(cookie, "berth_login=", ";").to_owned();
    let (code, _) = pair.split_once('.').unwrap();
    assert!(started.body.contains(&code[..4]) && started.body.contains("<svg"));
    let poll = |claim: &str| {
        Request::get("/login/phone/status")
            .header(header::COOKIE, format!("berth_login={claim}"))
            .header("hx-request", "true")
            .header(
                header::USER_AGENT,
                "Mozilla/5.0 (Windows NT 10.0) Chrome/150",
            )
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send(&state, poll(&pair)).await.status,
        StatusCode::NO_CONTENT
    );

    // The phone sees which browser asks, then approves once.
    let asked = send(
        &state,
        bearer_get(&format!("/v1/auth/web-login/{code}"), &phone),
    )
    .await
    .json();
    assert_eq!(
        asked["browser"], "Browser",
        "the test request has no user agent"
    );
    let anonymous = send(
        &state,
        Request::get(format!("/v1/auth/web-login/{code}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(anonymous.problem(), "unauthenticated");
    let approve = format!("/v1/auth/web-login/{}/approve", code.to_lowercase());
    let approved = send(&state, api("POST", &approve, &phone, None)).await;
    assert_eq!(approved.status, StatusCode::NO_CONTENT, "{}", approved.body);
    let again = send(&state, api("POST", &approve, &phone, None)).await;
    assert_eq!(again.json()["detail"], "web_login_used");

    // Only the browser with the claim gets the session, and only once.
    let stranger = send(&state, poll(&format!("{code}.not-the-claim"))).await;
    assert!(stranger.body.contains("/login/phone"), "{}", stranger.body);
    assert!(stranger.session().is_none());
    let signed_in = send(&state, poll(&pair)).await;
    let session = signed_in.session().expect("session cookie");
    assert_eq!(signed_in.location(), Some("/"));
    assert_eq!(
        send(&state, get("/", Some(&session))).await.status,
        StatusCode::OK
    );
    assert!(send(&state, poll(&pair)).await.session().is_none());
    assert_eq!(
        send(&state, bearer_get("/v1/auth/web-login/ABCD2345", &phone))
            .await
            .json()["detail"],
        "web_login_invalid"
    );
}

#[tokio::test]
async fn database_restores_from_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let data = dir.path().join("data");
    let backup = berth_server::backup::before_migrations(&state.db, &data, true)
        .await
        .unwrap()
        .unwrap();
    let name = backup.file_name().unwrap().to_str().unwrap().to_owned();

    // A change after the backup...
    let renamed = send(
        &state,
        post(
            "/settings",
            Some(&session),
            &[("name", "Renamed shelf"), ("lan_url", ""), ("wan_url", "")],
        ),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    assert_eq!(state.config.load().name, "Renamed shelf");

    // ...is undone by restoring it; the state before the restore is kept.
    let restored = send(
        &state,
        post(
            &format!("/settings/backup/{name}/restore"),
            Some(&session),
            &[],
        ),
    )
    .await;
    assert_eq!(
        restored.location(),
        Some("/settings/restore?restored=true"),
        "{}",
        restored.body
    );
    assert_eq!(state.config.load().name, "Family shelf");
    assert_eq!(berth_server::backup::list(&data).len(), 2);
    let page = send(
        &state,
        get("/settings/restore?restored=true", Some(&session)),
    )
    .await;
    assert!(
        page.body.contains("Family shelf"),
        "the session survived the restore"
    );

    // An uploaded file must be a Berth database not newer than the server.
    let upload = |bytes: &[u8]| {
        let mut body = b"--b\r\nContent-Disposition: form-data; name=\"backup\"; filename=\"x.sqlite\"\r\n\r\n".to_vec();
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n--b--\r\n");
        browser(
            "POST",
            "/settings/backup/restore",
            &session,
            "multipart/form-data; boundary=b",
            body,
        )
    };
    let junk = send(&state, upload(b"not a database")).await;
    assert!(junk.body.contains("alert-error"), "{}", junk.body);
    let newer = data.join("newer.sqlite");
    berth_server::backup::snapshot(&state.db, &newer)
        .await
        .unwrap();
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", newer.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time)
         VALUES (99999, 'future', CURRENT_TIMESTAMP, TRUE, x'00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let refused = send(&state, upload(&std::fs::read(&newer).unwrap())).await;
    assert!(
        refused
            .body
            .contains(&berth_server::backup::list(&data)[0].name)
            && refused.body.contains("alert-error"),
        "{}",
        refused.body
    );
    let good = send(&state, upload(&std::fs::read(&backup).unwrap())).await;
    assert_eq!(
        good.location(),
        Some("/settings/restore?restored=true"),
        "{}",
        good.body
    );
}

/// A fake Cloudflare API: accepts the token "good", records bucket and CORS calls; a second
/// bucket with the same name answers "already exists" (10004) like Cloudflare.
async fn fake_cloudflare() -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
) {
    use axum::{
        Json, Router,
        extract::State as S,
        http::{HeaderMap as H, Uri},
        routing::{get, post, put},
    };
    type Log = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;
    async fn verify(headers: H) -> (StatusCode, Json<serde_json::Value>) {
        if headers[header::AUTHORIZATION] == "Bearer good" {
            (
                StatusCode::OK,
                Json(
                    serde_json::json!({ "success": true, "result": { "id": "tok-id", "status": "active" } }),
                ),
            )
        } else {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "success": false, "errors": [{ "code": 1000 }] })),
            )
        }
    }
    async fn record(S(log): S<Log>, uri: Uri, body: String) -> Json<serde_json::Value> {
        let mut log = log.lock().unwrap();
        let seen = log.iter().any(|(u, b)| *u == uri.path() && *b == body);
        log.push((uri.path().to_owned(), body));
        if seen && uri.path().ends_with("/buckets") {
            return Json(serde_json::json!({ "success": false, "errors": [{ "code": 10004 }] }));
        }
        Json(serde_json::json!({ "success": true, "result": {} }))
    }
    let log: Log = std::sync::Arc::default();
    let app = Router::new()
        .route("/client/v4/accounts/{account}/tokens/verify", get(verify))
        .route("/client/v4/user/tokens/verify", get(verify))
        .route("/client/v4/accounts/{account}/r2/buckets", post(record))
        .route(
            "/client/v4/accounts/{account}/r2/buckets/{bucket}/cors",
            put(record),
        )
        .with_state(log.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/client/v4"), log)
}

#[tokio::test]
async fn r2_wizard_creates_bucket_cors_and_profile() {
    const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
    use berth_server::service::{r2, storage};
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let tenant_id = tenant(&state).await;
    let (api, log) = fake_cloudflare().await;
    let input = |token: &str, name: &str| r2::R2Input {
        name: name.into(),
        account_id: ACCOUNT.into(),
        token: token.into(),
        bucket: "berth-books".into(),
    };

    let refused = r2::connect(&state, &tenant_id, &input("bad", ""), &api, vec![]).await;
    assert!(
        matches!(refused, Err(berth_core::BerthError::Validation(c)) if c == "r2_token_invalid")
    );
    assert!(
        log.lock().unwrap().is_empty(),
        "nothing is created with a bad token"
    );

    let id = r2::connect(&state, &tenant_id, &input("good", ""), &api, vec![])
        .await
        .unwrap();
    let profile = storage::get(&state.db, &tenant_id, &id).await.unwrap();
    assert_eq!(profile.name, "Cloudflare R2");
    assert_eq!(profile.kind, "s3");
    assert_eq!(
        profile.endpoint,
        format!("https://{ACCOUNT}.r2.cloudflarestorage.com")
    );
    assert_eq!(
        (profile.bucket.as_str(), profile.region.as_str()),
        ("berth-books", "auto")
    );
    assert_eq!(profile.delivery_mode, "presigned");
    {
        let log = log.lock().unwrap();
        assert_eq!(
            log[0].0,
            format!("/client/v4/accounts/{ACCOUNT}/r2/buckets")
        );
        assert_eq!(log[0].1, r#"{"name":"berth-books"}"#);
        let cors: serde_json::Value = serde_json::from_str(&log[1].1).unwrap();
        assert_eq!(
            cors["rules"][0]["allowed"]["origins"],
            serde_json::json!(["*"])
        );
        assert!(
            cors["rules"][0]["allowed"]["methods"]
                .as_array()
                .unwrap()
                .contains(&"PUT".into())
        );
    }

    // The same bucket again ("already exists") is fine.
    r2::connect(&state, &tenant_id, &input("good", "R2 again"), &api, vec![])
        .await
        .unwrap();

    // The admin dialog reopens with the error and without the token.
    let page = send(
        &state,
        post(
            "/storage/r2",
            Some(&session),
            &[
                ("account_id", "123"),
                ("token", "secret-token"),
                ("bucket", "berth"),
                ("name", ""),
            ],
        ),
    )
    .await;
    assert!(
        page.body.contains("r2-storage") && page.body.contains("alert-error"),
        "{}",
        page.body
    );
    assert!(!page.body.contains("secret-token"));
}

// ── Login for apps and per-user access ──────────────────

/// A book of one short chapter, saved through the ingest API.
async fn add_book(state: &AppState, token: &str, title: &str) -> String {
    let mp3 = chapter_mp3(title, 1, 1);
    let created = send(
        state,
        api(
            "POST",
            "/v1/ingest/uploads",
            token,
            Some(&serde_json::json!({ "name": title, "files": [{ "name": "a.mp3", "size": mp3.len() }] })),
        ),
    )
    .await
    .json();
    let id = created["id"].as_str().unwrap().to_owned();
    let uploads = format!("/v1/ingest/uploads/{id}");
    send(state, put_bytes(&format!("{uploads}/files/0"), token, mp3)).await;
    send(
        state,
        api("POST", &format!("{uploads}/complete"), token, None),
    )
    .await;
    berth_server::jobs::run_due(state).await.unwrap();
    let saved = send(
        state,
        api(
            "POST",
            &format!("{uploads}/save"),
            token,
            Some(&serde_json::json!({ "title": title, "authors": [], "narrators": [], "series": "" })),
        ),
    )
    .await;
    assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.body);
    saved.json()["book_id"].as_str().unwrap().to_owned()
}

fn app_login(login: &str, password: &str) -> Request<Body> {
    json(
        "/v1/auth/login",
        None,
        &serde_json::json!({ "login": login, "password": password, "device_name": "Review", "platform": "ios" }),
    )
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one journey: login, picked books, access change, login off"
)]
async fn apps_sign_in_with_a_login_and_see_only_picked_books() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let admin = admin_token(&state).await;
    let open = add_book(&state, &admin, "Открытая").await;
    let hidden = add_book(&state, &admin, "Закрытая").await;
    let modes =
        || async { send(&state, get("/v1/server-info", None)).await.json()["auth_modes"].clone() };
    assert_eq!(modes().await, serde_json::json!(["pairing"]));

    send(
        &state,
        post("/users", Some(&session), &[("name", "Модерация")]),
    )
    .await;
    let user: String = sqlx::query_scalar("SELECT id FROM users WHERE name = 'Модерация'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let settings = format!("/users/{user}/settings");
    let dialog = send(&state, get(&settings, Some(&session))).await;
    assert!(dialog.body.contains("Закрытая"), "{}", dialog.body);
    let short = send(
        &state,
        post(
            &settings,
            Some(&session),
            &[
                ("login_on", "on"),
                ("login", "demo"),
                ("password", "dem"),
                ("access", "all"),
            ],
        ),
    )
    .await;
    assert!(
        short.body.contains("role=\"alert\"") && short.body.contains("id=\"user-settings\""),
        "the dialog comes back with the error: {}",
        short.body
    );
    let saved = send(
        &state,
        post(
            &settings,
            Some(&session),
            &[
                ("login_on", "on"),
                ("login", "demo"),
                ("password", "demo"),
                ("access", "selected"),
                ("books", &open),
            ],
        ),
    )
    .await;
    assert!(!saved.body.contains("role=\"alert\""), "{}", saved.body);
    assert!(saved.body.contains("demo"), "the login chip");
    assert_eq!(modes().await, serde_json::json!(["pairing", "password"]));

    // Wrong password, then the login in other case.
    assert_eq!(
        send(&state, app_login("demo", "nope")).await.problem(),
        "login_failed"
    );
    assert_eq!(
        send(&state, app_login("nobody", "demo")).await.problem(),
        "login_failed"
    );
    let signed = send(&state, app_login("DEMO", "demo")).await;
    assert_eq!(signed.status, StatusCode::OK, "{}", signed.body);
    let signed = signed.json();
    let token = signed["access_token"].as_str().unwrap().to_owned();
    let scopes: Vec<String> =
        sqlx::query_scalar("SELECT scope FROM device_scopes WHERE device_id = $1")
            .bind(signed["device_id"].as_str().unwrap())
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(scopes, ["listen"]);

    // Only the picked book, everywhere.
    let page = send(&state, bearer_get("/v1/sync", &token)).await.json();
    let ids: Vec<&str> = page["books"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [open.as_str()]);
    assert_eq!(page["reset"], true);
    for path in [
        format!("/v1/books/{hidden}/files/0/stream"),
        format!("/v1/books/{hidden}/download-manifest"),
    ] {
        assert_eq!(
            send(&state, bearer_get(&path, &token)).await.problem(),
            "not_found",
            "{path}"
        );
    }
    let stream = send(
        &state,
        bearer_get(&format!("/v1/books/{open}/files/0/stream"), &token),
    )
    .await;
    assert_eq!(stream.status, StatusCode::OK, "{}", stream.body);

    // Opening the whole library changes the policy: the next sync is a full slice.
    let cursor = page["cursor"].as_str().unwrap().to_owned();
    send(
        &state,
        post(
            &settings,
            Some(&session),
            &[
                ("login_on", "on"),
                ("login", "demo"),
                ("password", ""),
                ("access", "all"),
            ],
        ),
    )
    .await;
    let page = send(
        &state,
        bearer_get(&format!("/v1/sync?cursor={cursor}"), &token),
    )
    .await
    .json();
    assert_eq!(page["reset"], true);
    assert_eq!(page["books"].as_array().unwrap().len(), 2);
    // An empty password kept the saved one.
    assert_eq!(
        send(&state, app_login("demo", "demo")).await.status,
        StatusCode::OK
    );

    // The owner cannot get a login; turning the listener's login off closes it.
    let owner: String = sqlx::query_scalar("SELECT id FROM users WHERE role = 'owner'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let tenant_id = tenant(&state).await;
    let refused = berth_server::service::users::set_login(
        &state.db,
        &tenant_id,
        &owner,
        "boss",
        "long enough",
    )
    .await;
    assert!(matches!(
        refused,
        Err(berth_core::BerthError::Validation(code)) if code == "login_admin"
    ));
    send(
        &state,
        post(&settings, Some(&session), &[("access", "all")]),
    )
    .await;
    assert_eq!(modes().await, serde_json::json!(["pairing"]));
    assert_eq!(
        send(&state, app_login("demo", "demo")).await.problem(),
        "login_failed"
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one journey: languages of books, the filter, a listener's languages"
)]
async fn listeners_see_only_books_in_their_languages() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let admin = admin_token(&state).await;
    let russian = add_book(&state, &admin, "Сами боги").await;
    let english = add_book(&state, &admin, "Foundation").await;
    let unknown = add_book(&state, &admin, "Untitled").await;
    let set_language = |id: String, title: &'static str, language: &'static str| {
        let state = &state;
        let session = &session;
        async move {
            let saved = send(
                state,
                post(
                    &format!("/book/{id}"),
                    Some(session),
                    &[("title", title), ("language", language)],
                ),
            )
            .await;
            assert!(!saved.body.contains("alert-error"), "{}", saved.body);
        }
    };
    set_language(english.clone(), "Foundation", "en").await;
    set_language(unknown.clone(), "Untitled", "").await;
    let wrong = send(
        &state,
        post(
            &format!("/book/{english}"),
            Some(&session),
            &[("title", "Foundation"), ("language", "xx")],
        ),
    )
    .await;
    assert!(wrong.body.contains("alert-error"), "{}", wrong.body);

    // The library filters by language once there are two.
    let library: String = sqlx::query_scalar("SELECT library_id FROM books LIMIT 1")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let page = send(&state, get(&format!("/library/{library}"), Some(&session))).await;
    assert!(page.body.contains("name=\"language\""), "{}", page.body);
    let only = send(
        &state,
        get(&format!("/library/{library}?language=en"), Some(&session)),
    )
    .await;
    assert!(only.body.contains(&format!("/book/{english}")));
    assert!(!only.body.contains(&format!("/book/{russian}")));

    // A listener who reads English: no Russian book, the one without a language stays.
    send(
        &state,
        post("/users", Some(&session), &[("name", "Reader")]),
    )
    .await;
    let user: String = sqlx::query_scalar("SELECT id FROM users WHERE name = 'Reader'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let settings = format!("/users/{user}/settings");
    let saved = send(
        &state,
        post(
            &settings,
            Some(&session),
            &[
                ("login_on", "on"),
                ("login", "reader"),
                ("password", "reader"),
                ("access", "all"),
                ("languages", "en"),
            ],
        ),
    )
    .await;
    assert!(!saved.body.contains("role=\"alert\""), "{}", saved.body);
    let token = send(&state, app_login("reader", "reader")).await.json()["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let page = send(&state, bearer_get("/v1/sync", &token)).await.json();
    let mut ids: Vec<&str> = page["books"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    let mut expected = [english.as_str(), unknown.as_str()];
    expected.sort_unstable();
    assert_eq!(ids, expected);
    let stream = send(
        &state,
        bearer_get(&format!("/v1/books/{russian}/files/0/stream"), &token),
    )
    .await;
    assert_eq!(stream.problem(), "not_found");

    // A book that turns Russian leaves the phone on the next sync.
    let cursor = page["cursor"].as_str().unwrap().to_owned();
    set_language(english.clone(), "Foundation", "ru").await;
    let next = send(
        &state,
        bearer_get(&format!("/v1/sync?cursor={cursor}"), &token),
    )
    .await
    .json();
    assert_eq!(next["removed"], serde_json::json!([english]));
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one journey: a closed section, a given one, the owner, the languages tab"
)]
async fn closed_and_given_sections_and_the_languages_tab() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let admin = admin_token(&state).await;
    let open = add_book(&state, &admin, "Открытая").await;
    let secret = add_book(&state, &admin, "Только для демо").await;
    let fresh = add_book(&state, &admin, "Новая сказка").await;

    for (name, login) in [("Мама", "mama"), ("Demo", "demo")] {
        send(&state, post("/users", Some(&session), &[("name", name)])).await;
        let user: String = sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(name)
            .fetch_one(&state.db)
            .await
            .unwrap();
        send(
            &state,
            post(
                &format!("/users/{user}/settings"),
                Some(&session),
                &[
                    ("login_on", "on"),
                    ("login", login),
                    ("password", login),
                    ("access", "all"),
                ],
            ),
        )
        .await;
    }
    let demo: String = sqlx::query_scalar("SELECT id FROM users WHERE name = 'Demo'")
        .fetch_one(&state.db)
        .await
        .unwrap();

    // The dialog of a new section, then a closed one open to Demo only.
    let dialog = send(&state, get("/categories/new", Some(&session))).await;
    assert!(
        dialog.body.contains("<dialog") && dialog.body.contains("Demo"),
        "{}",
        dialog.body
    );
    let added = send(
        &state,
        post(
            "/categories",
            Some(&session),
            &[("name", "Демо"), ("closed", "on"), ("users", &demo)],
        ),
    )
    .await;
    assert!(!added.body.contains("alert-error"), "{}", added.body);
    send(
        &state,
        post("/categories", Some(&session), &[("name", "Сказки")]),
    )
    .await;
    let section = |name: &'static str| {
        let state = &state;
        async move {
            sqlx::query_scalar::<_, String>("SELECT id FROM categories WHERE name = $1")
                .bind(name)
                .fetch_one(&state.db)
                .await
                .unwrap()
        }
    };
    let closed = section("Демо").await;
    let tales = section("Сказки").await;
    let shelve = |book: String, title: &'static str, category: String| {
        let state = &state;
        let session = &session;
        async move {
            send(
                state,
                post(
                    &format!("/book/{book}"),
                    Some(session),
                    &[
                        ("title", title),
                        ("language", "ru"),
                        ("category_ids", &category),
                    ],
                ),
            )
            .await;
        }
    };
    shelve(
        secret.clone(),
        "Только для демо",
        format!("{closed},{tales}"),
    )
    .await;
    shelve(fresh.clone(), "Новая сказка", tales.clone()).await;

    let token = |login: &'static str| {
        let state = &state;
        async move {
            send(state, app_login(login, login)).await.json()["access_token"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let synced = |token: String| {
        let state = &state;
        async move {
            let page = send(state, bearer_get("/v1/sync", &token)).await.json();
            let mut books: Vec<String> = page["books"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["id"].as_str().unwrap().to_owned())
                .collect();
            books.sort_unstable();
            let sections: Vec<String> = page["categories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap().to_owned())
                .collect();
            (books, sections)
        }
    };
    let sorted = |mut ids: Vec<&String>| {
        ids.sort_unstable();
        ids.into_iter().cloned().collect::<Vec<_>>()
    };

    // Mama sees the whole library, but not the closed section, even through an open one.
    let mama = token("mama").await;
    let (books, sections) = synced(mama.clone()).await;
    assert_eq!(books, sorted(vec![&open, &fresh]));
    assert_eq!(
        sections,
        ["Сказки"],
        "the closed section's name does not leak"
    );
    assert_eq!(
        send(
            &state,
            bearer_get(&format!("/v1/books/{secret}/files/0/stream"), &mama)
        )
        .await
        .problem(),
        "not_found"
    );
    // Demo has it.
    let (books, sections) = synced(token("demo").await).await;
    assert_eq!(books, sorted(vec![&open, &secret, &fresh]));
    assert_eq!(sections, ["Демо", "Сказки"]);
    // The owner sees everything.
    let (books, _) = synced(admin.clone()).await;
    assert_eq!(books.len(), 3);

    // Demo on "selected" with the closed section given: only its books, a new one too.
    send(
        &state,
        post(
            &format!("/users/{demo}/settings"),
            Some(&session),
            &[
                ("login_on", "on"),
                ("login", "demo"),
                ("password", ""),
                ("access", "selected"),
                ("categories", &closed),
            ],
        ),
    )
    .await;
    let demo_token = token("demo").await;
    let (books, _) = synced(demo_token.clone()).await;
    assert_eq!(books, std::slice::from_ref(&secret));
    shelve(open.clone(), "Открытая", closed.clone()).await;
    let (books, _) = synced(demo_token).await;
    assert_eq!(
        books,
        sorted(vec![&open, &secret]),
        "a new book of the section"
    );

    // Opening the section to everyone gives Mama its books on a full slice.
    let edit = send(
        &state,
        get(&format!("/categories/{closed}/edit"), Some(&session)),
    )
    .await;
    assert!(edit.body.contains("checked"), "{}", edit.body);
    send(
        &state,
        post(
            &format!("/categories/{closed}"),
            Some(&session),
            &[("name", "Демо"), ("users", &demo)],
        ),
    )
    .await;
    let page = send(&state, bearer_get("/v1/sync?cursor=v1.0.0", &mama))
        .await
        .json();
    assert_eq!(page["reset"], true);
    assert_eq!(page["books"].as_array().unwrap().len(), 3);

    // Languages: add one, a book's language cannot go, an unused one can.
    let tab = send(&state, get("/settings/languages", Some(&session))).await;
    assert!(tab.body.contains("Русский") && tab.body.contains("English"));
    let added = send(
        &state,
        post(
            "/settings/languages",
            Some(&session),
            &[("code", "Pt"), ("name", "Português")],
        ),
    )
    .await;
    assert!(added.body.contains("Português"), "{}", added.body);
    let again = send(
        &state,
        post(
            "/settings/languages",
            Some(&session),
            &[("code", "pt"), ("name", "Portuguese")],
        ),
    )
    .await;
    assert!(again.body.contains("alert-error"), "{}", again.body);
    send(
        &state,
        post("/settings/languages/ru/delete", Some(&session), &[]),
    )
    .await;
    send(
        &state,
        post("/settings/languages/pt/delete", Some(&session), &[]),
    )
    .await;
    let codes: Vec<String> = sqlx::query_scalar("SELECT code FROM languages ORDER BY position")
        .fetch_all(&state.db)
        .await
        .unwrap();
    assert!(codes.contains(&"ru".to_owned()), "books are in Russian");
    assert!(!codes.contains(&"pt".to_owned()));
    let page = send(&state, bearer_get("/v1/sync", &admin)).await.json();
    assert_eq!(page["languages"][0]["name"], "Русский");
}

// ── Demo library ─────────────────────────────────────────

/// A stand-in for archive.org: every item has seven short MP3s and a public-domain license,
/// except `chelovek_2103_librivox`, which is under CC BY.
async fn fake_archive() -> String {
    use axum::{Json, Router, extract::Path as UrlPath, routing::get as route_get};
    fn mp3() -> Vec<u8> {
        fixtures::mp3(&fixtures::Mp3 {
            seconds: 1,
            title: Some("Глава"),
            album: Some("Книга"),
            artist: Some("Автор"),
            narrator: Some("Чтец LibriVox"),
            track: None,
            genre: None,
            language: None,
            chapters: &[],
            cover: false,
        })
    }
    let app = Router::new()
        .route(
            "/metadata/{item}",
            route_get(|UrlPath(item): UrlPath<String>| async move {
                let license = if item == "chelovek_2103_librivox" {
                    "https://creativecommons.org/licenses/by/4.0/"
                } else {
                    "http://creativecommons.org/publicdomain/mark/1.0/"
                };
                let size = mp3().len().to_string();
                let files: Vec<serde_json::Value> = (1..=7)
                    .map(|i| {
                        serde_json::json!({ "name": format!("{item}_{i:02}_64kb.mp3"), "format": "64Kbps MP3", "size": size })
                    })
                    .chain([serde_json::json!({ "name": "x.ogg", "format": "Ogg Vorbis" })])
                    .collect();
                Json(serde_json::json!({ "metadata": { "licenseurl": license }, "files": files }))
            }),
        )
        .route(
            "/download/{item}/{name}",
            route_get(|| async { mp3() }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one journey: load, review listener, whole books, remove"
)]
async fn the_demo_library_loads_changes_and_goes() {
    let dir = tempfile::tempdir().unwrap();
    let source = fake_archive().await;
    let config = dir.path().join("berth.toml");
    std::fs::write(
        &config,
        format!(
            "data_dir = '{}'\ndemo_source = '{source}'\n",
            dir.path().join("data").display()
        ),
    )
    .unwrap();
    let state = berth_server::bootstrap(Some(&config)).await.unwrap();
    let session = set_up(&state).await;
    let admin = admin_token(&state).await;

    // A listener of the family, who must not see a closed demo.
    send(
        &state,
        post("/users", Some(&session), &[("name", "Reader")]),
    )
    .await;
    let reader: String = sqlx::query_scalar("SELECT id FROM users WHERE name = 'Reader'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    send(
        &state,
        post(
            &format!("/users/{reader}/settings"),
            Some(&session),
            &[
                ("login_on", "on"),
                ("login", "reader"),
                ("password", "reader"),
                ("access", "all"),
            ],
        ),
    )
    .await;

    let tab = send(&state, get("/settings/demo", Some(&session))).await;
    assert!(tab.body.contains("Белые ночи"), "{}", tab.body);
    let missing = send(
        &state,
        post(
            "/settings/demo",
            Some(&session),
            &[("slots", ""), ("length", "short")],
        ),
    )
    .await;
    assert!(missing.body.contains("alert-error"), "{}", missing.body);
    // One book picked alone first, then the whole Russian set.
    send(
        &state,
        post(
            "/settings/demo",
            Some(&session),
            &[("slots", "ru-01"), ("length", "short")],
        ),
    )
    .await;
    berth_server::jobs::run_due(&state).await.unwrap();
    let picked: Vec<String> = sqlx::query_scalar("SELECT slot FROM demo_books")
        .fetch_all(&state.db)
        .await
        .unwrap();
    assert_eq!(picked, ["ru-01"], "one book picked, one loaded");
    let russian: Vec<String> = berth_server::service::demo::catalog()
        .into_iter()
        .filter(|b| b.locale == "ru")
        .map(|b| b.slot)
        .collect();
    let russian = russian.join(",");
    let applied = send(
        &state,
        post(
            "/settings/demo",
            Some(&session),
            &[
                ("slots", &russian),
                ("length", "short"),
                ("closed", "on"),
                ("reviewer", "on"),
                ("reviewer_name", "Модерация"),
                ("reviewer_login", "review"),
                ("reviewer_password", "review"),
            ],
        ),
    )
    .await;
    assert!(!applied.body.contains("alert-error"), "{}", applied.body);
    berth_server::jobs::run_due(&state).await.unwrap();

    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT slot, status FROM demo_books ORDER BY slot")
            .fetch_all(&state.db)
            .await
            .unwrap();
    assert_eq!(statuses.len(), 9, "the Russian set");
    assert_eq!(
        statuses.iter().filter(|(_, s)| s == "ready").count(),
        8,
        "{statuses:?}"
    );
    assert!(
        statuses.contains(&("ru-09".into(), "failed".into())),
        "not public domain"
    );

    let ids = |page: &serde_json::Value| -> Vec<String> {
        page["books"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let token = |login: &'static str, password: &'static str| {
        let state = &state;
        async move {
            send(state, app_login(login, password)).await.json()["access_token"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let review = token("review", "review").await;
    let page = send(&state, bearer_get("/v1/sync", &review)).await.json();
    assert_eq!(ids(&page).len(), 8);
    assert_eq!(page["categories"][0]["name"], "Demo");
    let book = page["books"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["title"] == "Белые ночи")
        .unwrap()
        .clone();
    assert_eq!(book["files"].as_array().unwrap().len(), 3, "first chapters");
    assert_eq!(book["language"], "ru");
    let reader_token = token("reader", "reader").await;
    let page = send(&state, bearer_get("/v1/sync", &reader_token))
        .await
        .json();
    assert!(ids(&page).is_empty(), "closed to the family");
    let page = send(&state, bearer_get("/v1/sync", &admin)).await.json();
    assert_eq!(ids(&page).len(), 8, "the owner sees it");

    // Whole books replace the short ones; the review password stays when left empty.
    send(
        &state,
        post(
            "/settings/demo",
            Some(&session),
            &[
                ("slots", &russian),
                ("length", "full"),
                ("closed", "on"),
                ("reviewer", "on"),
                ("reviewer_name", "Модерация"),
                ("reviewer_login", "review"),
                ("reviewer_password", ""),
            ],
        ),
    )
    .await;
    berth_server::jobs::run_due(&state).await.unwrap();
    let page = send(&state, bearer_get("/v1/sync", &review)).await.json();
    assert_eq!(ids(&page).len(), 8, "replaced, not doubled");
    let whole = page["books"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["title"] == "Белые ночи")
        .unwrap()
        .clone();
    assert_eq!(whole["files"].as_array().unwrap().len(), 7);
    assert_ne!(whole["id"], book["id"]);
    let tab = send(&state, get("/settings/demo", Some(&session))).await;
    assert!(tab.body.contains("Модерация"), "{}", tab.body);

    // Removed: no demo books, no section, no review login.
    let removed = send(&state, post("/settings/demo/remove", Some(&session), &[])).await;
    assert!(!removed.body.contains("alert-error"), "{}", removed.body);
    let books: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM books WHERE deleted_at IS NULL")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(books, 0);
    let sections: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM categories")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(sections, 0);
    assert_eq!(
        send(&state, app_login("review", "review")).await.problem(),
        "login_failed"
    );
}

/// Uploads a one-file book and saves it with `title`; its id.
async fn upload_book(state: &AppState, session: &str, title: &str) -> String {
    let m4b = fixtures::m4b(&fixtures::M4b {
        seconds: 60,
        title,
        artist: "Айзек Азимов",
        genre: None,
        language: None,
        track_language: None,
        chapters: &[],
    });
    let new = serde_json::json!({ "name": format!("{title}.m4b"), "files": [{ "name": "a.m4b", "size": m4b.len() }] });
    let created = send(
        state,
        browser(
            "POST",
            "/upload/new",
            session,
            "application/json",
            new.to_string().into_bytes(),
        ),
    )
    .await;
    let id = created.json()["id"].as_str().unwrap().to_owned();
    send(
        state,
        browser(
            "PUT",
            &format!("/upload/{id}/files/0"),
            session,
            "application/octet-stream",
            m4b,
        ),
    )
    .await;
    send(
        state,
        post(&format!("/upload/{id}/complete"), Some(session), &[]),
    )
    .await;
    berth_server::jobs::run_due(state).await.unwrap();
    let saved = send(
        state,
        post(
            &format!("/upload/{id}/save"),
            Some(session),
            &[("title", title), ("authors", "Айзек Азимов")],
        ),
    )
    .await;
    between(&saved.body, "href=\"/book/", "\"").to_owned()
}

/// A stand-in OpenAI-compatible API: knows "Основание", nothing else; its address.
async fn mock_ai() -> String {
    use axum::{Router, routing::post as route_post};
    async fn complete(body: String) -> axum::Json<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body).unwrap();
        let user = request["messages"][1]["content"].as_str().unwrap();
        let answer = if user.contains("Основание") {
            assert!(user.contains("Language: Russian"), "{user}");
            serde_json::json!({ "description": "Сага о падении Галактической империи.", "year": 1951 })
        } else {
            serde_json::json!({ "description": null, "year": null })
        };
        axum::Json(
            serde_json::json!({ "choices": [{ "message": { "content": answer.to_string() } }] }),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/chat/completions", route_post(complete)),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}/v1")
}

async fn about_of(state: &AppState, id: &str) -> (Option<String>, Option<i64>) {
    sqlx::query_as("SELECT description, year FROM books WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one journey: the fields, the dialog, sync, then the AI"
)]
async fn books_have_a_description_and_the_ai_fills_empty_ones() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let book = upload_book(&state, &session, "Основание").await;
    let other = upload_book(&state, &session, "Неизвестная книга").await;

    // The admin writes them; a wrong year is refused.
    let wrong = send(
        &state,
        post(
            &format!("/book/{book}"),
            Some(&session),
            &[("title", "Основание"), ("year", "abc")],
        ),
    )
    .await;
    assert!(wrong.body.contains("role=\"alert\""), "{}", wrong.body);
    let saved = send(
        &state,
        post(
            &format!("/book/{book}"),
            Some(&session),
            &[
                ("title", "Основание"),
                ("authors", "Айзек Азимов"),
                ("language", "ru"),
                ("year", "1942"),
                ("description", "Моё описание"),
            ],
        ),
    )
    .await;
    assert!(saved.body.contains("Моё описание"), "{}", saved.body);
    let library = send(&state, get("/library", Some(&session))).await;
    let grid = send(&state, get(library.location().unwrap(), Some(&session))).await;
    assert!(grid.body.contains(&format!("/book/{book}/about")));
    assert!(
        !grid.body.contains(&format!("/book/{other}/about")),
        "no button without a description"
    );
    let about = send(&state, get(&format!("/book/{book}/about"), Some(&session))).await;
    assert!(
        about.body.contains("Моё описание") && about.body.contains("1942"),
        "{}",
        about.body
    );

    // Phones get both.
    let tenant: String = sqlx::query_scalar("SELECT id FROM tenants")
        .fetch_one(&state.db)
        .await
        .unwrap();
    let page = berth_server::service::books::sync(
        &state.db,
        &tenant,
        &berth_server::service::books::PolicyFilter::admin(),
        None,
    )
    .await
    .unwrap();
    let page = serde_json::to_value(&page).unwrap();
    let synced = page["books"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == book.as_str())
        .unwrap();
    assert_eq!(
        (synced["description"].as_str(), synced["year"].as_i64()),
        (Some("Моё описание"), Some(1942))
    );

    // The AI fills what is empty and keeps the admin's text.
    send(
        &state,
        post(
            &format!("/book/{book}"),
            Some(&session),
            &[
                ("title", "Основание"),
                ("authors", "Айзек Азимов"),
                ("language", "ru"),
                ("description", "Моё описание"),
            ],
        ),
    )
    .await;
    let not_ready = send(&state, post("/settings/ai/run", Some(&session), &[])).await;
    assert!(
        not_ready.body.contains("role=\"alert\""),
        "{}",
        not_ready.body
    );
    let url = mock_ai().await;
    let configured = send(
        &state,
        post(
            "/settings/ai",
            Some(&session),
            &[("base_url", &url), ("model", "test"), ("api_key", "sk-1")],
        ),
    )
    .await;
    assert!(
        configured.body.contains("role=\"status\""),
        "{}",
        configured.body
    );
    let stored: String =
        sqlx::query_scalar("SELECT value_encrypted FROM settings WHERE key = 'ai_key'")
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert!(!stored.contains("sk-1"), "the key is stored encrypted");
    let checked = send(&state, post("/settings/ai/check", Some(&session), &[])).await;
    assert!(checked.body.contains("alert-ok"), "{}", checked.body);

    let started = send(&state, post("/settings/ai/run", Some(&session), &[])).await;
    assert!(
        started.body.contains("/settings/ai/status"),
        "{}",
        started.body
    );
    berth_server::jobs::run_due(&state).await.unwrap();
    assert_eq!(
        about_of(&state, &book).await,
        (Some("Моё описание".into()), Some(1951))
    );
    assert_eq!(about_of(&state, &other).await, (None, None));
    let status = send(&state, get("/settings/ai/status", Some(&session))).await;
    assert!(
        !status.body.contains("hx-trigger") && status.body.contains("alert-ok"),
        "the run is over: {}",
        status.body
    );

    // The book page fills an empty description into the form, without saving.
    let filled = send(
        &state,
        post(
            &format!("/book/{book}/ai"),
            Some(&session),
            &[
                ("title", "Основание"),
                ("authors", "Айзек Азимов"),
                ("language", "ru"),
            ],
        ),
    )
    .await;
    assert!(
        filled
            .body
            .contains("Сага о падении Галактической империи."),
        "{}",
        filled.body
    );
    assert_eq!(
        about_of(&state, &book).await.0.as_deref(),
        Some("Моё описание")
    );
}

#[tokio::test]
async fn the_queue_keeps_saved_uploads_a_day_and_the_history_pages_them() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let book = upload_book(&state, &session, "Основание").await;
    // Saved two days ago, plus eleven more saved uploads of that book.
    let old = 1_000_000;
    sqlx::query("UPDATE uploads SET updated_at = $1, created_at = $1")
        .bind(old)
        .execute(&state.db)
        .await
        .unwrap();
    for n in 0..11 {
        sqlx::query(
            "INSERT INTO uploads (id, tenant_id, library_id, storage_id, name, status, book_id,
                created_at, updated_at)
             SELECT $1, tenant_id, library_id, storage_id, $2, 'saved', book_id, $3, $3 FROM uploads LIMIT 1",
        )
        .bind(format!("old-{n}"))
        .bind(format!("Старая {n}"))
        .bind(old + n)
        .execute(&state.db)
        .await
        .unwrap();
    }

    let queue = send(&state, get("/upload", Some(&session))).await;
    assert!(
        !queue.body.contains(&format!("/book/{book}")),
        "out of the queue after a day"
    );
    assert!(queue.body.contains("view=history"), "the history tab");
    let first = send(&state, get("/upload?view=history", Some(&session))).await;
    assert!(
        first.body.contains("Старая 10") && !first.body.contains("Основание"),
        "newest first, ten a page: {}",
        first.body
    );
    assert!(first.body.contains("page=2"), "{}", first.body);
    let second = send(&state, get("/upload?view=history&page=2", Some(&session))).await;
    assert!(
        second.body.contains("Основание") && second.body.contains("Старая 0"),
        "{}",
        second.body
    );
    let past = send(&state, get("/upload?view=history&page=9", Some(&session))).await;
    assert!(
        past.body.contains("Основание"),
        "past the end: the last page"
    );
}

/// 1 January of the year `age` years before this one: the child is `age` all year round.
fn born_years_ago(age: i64) -> String {
    use berth_server::service::books::birth_days;
    let today = now().div_euclid(86_400_000);
    let year = (1970..2200)
        .find(|y| birth_days(&format!("{}-01-01", y + 1)).unwrap() > today)
        .unwrap();
    format!("{}-01-01", year - age)
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one journey: groups of a family, a child's age, the store reviewer outside groups"
)]
async fn groups_and_age_ratings() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let session = set_up(&state).await;
    let admin = admin_token(&state).await;
    let tenant_id = tenant(&state).await;
    let tale = add_book(&state, &admin, "Колобок").await;
    let teen = add_book(&state, &admin, "Гарри Поттер").await;
    let unrated = add_book(&state, &admin, "Без рейтинга").await;
    let novel = add_book(&state, &admin, "Мастер и Маргарита").await;
    let report = add_book(&state, &admin, "Отчёт").await;
    // Ratings straight in the database: the book form is tested elsewhere.
    for (id, rating) in [
        (&tale, Some(0)),
        (&teen, Some(12)),
        (&unrated, None),
        (&novel, Some(16)),
        (&report, Some(18)),
    ] {
        sqlx::query("UPDATE books SET rating = $2 WHERE id = $1")
            .bind(id)
            .bind(rating)
            .execute(&state.db)
            .await
            .unwrap();
    }
    for (name, closed) in [("Детское", false), ("Рабочие", true)] {
        let mut form = vec![("name", name)];
        if closed {
            form.push(("closed", "on"));
        }
        let added = send(&state, post("/categories", Some(&session), &form)).await;
        assert!(!added.body.contains("alert-error"), "{}", added.body);
    }
    let section = |name: &'static str| {
        let state = &state;
        async move {
            sqlx::query_scalar::<_, String>("SELECT id FROM categories WHERE name = $1")
                .bind(name)
                .fetch_one(&state.db)
                .await
                .unwrap()
        }
    };
    let kids = section("Детское").await;
    let work = section("Рабочие").await;
    for (book, category) in [
        (&tale, &kids),
        (&teen, &kids),
        (&unrated, &kids),
        (&report, &work),
    ] {
        sqlx::query(
            "INSERT INTO book_categories (tenant_id, book_id, category_id) VALUES ($1, $2, $3)",
        )
        .bind(&tenant_id)
        .bind(book)
        .bind(category)
        .execute(&state.db)
        .await
        .unwrap();
    }

    // Listeners with logins; `settings` saves their dialog (the login is sent each time).
    let mut ids = std::collections::HashMap::new();
    for name in ["Мама", "Папа", "Маша", "Модерация"] {
        send(&state, post("/users", Some(&session), &[("name", name)])).await;
        let id: String = sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
            .bind(name)
            .fetch_one(&state.db)
            .await
            .unwrap();
        ids.insert(name, id);
    }
    let login = |name: &str| match name {
        "Мама" => "mama",
        "Папа" => "papa",
        "Маша" => "masha",
        _ => "review",
    };
    let settings = |name: &'static str, extra: Vec<(&'static str, String)>| {
        let (state, session, ids) = (&state, &session, &ids);
        async move {
            let mut form = vec![
                ("login_on", "on".to_owned()),
                ("login", login(name).to_owned()),
                ("password", login(name).to_owned()),
            ];
            form.extend(extra);
            let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
            send(
                state,
                post(
                    &format!("/users/{}/settings", ids[name]),
                    Some(session),
                    &form,
                ),
            )
            .await
        }
    };
    for name in ["Мама", "Папа", "Маша"] {
        let saved = settings(name, vec![("access", "all".into())]).await;
        assert!(!saved.body.contains("role=\"alert\""), "{}", saved.body);
    }
    let reviewer = settings(
        "Модерация",
        vec![("access", "selected".into()), ("books", novel.clone())],
    )
    .await;
    assert!(
        !reviewer.body.contains("role=\"alert\""),
        "{}",
        reviewer.body
    );

    // Groups: adults see the library without closed sections, Work opens one to Papa,
    // Children see their section.
    let dialog = send(&state, get("/groups/new", Some(&session))).await;
    assert!(
        dialog.body.contains("id=\"group-settings\""),
        "{}",
        dialog.body
    );
    let group = |name: &'static str, form: Vec<(&'static str, String)>| {
        let (state, session) = (&state, &session);
        async move {
            let mut form = form;
            form.push(("name", name.to_owned()));
            let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
            send(state, post("/groups", Some(session), &form)).await
        }
    };
    let adults = group(
        "Взрослые",
        vec![
            ("access", "all".into()),
            ("members", format!("{},{}", ids["Мама"], ids["Папа"])),
        ],
    )
    .await;
    assert!(!adults.body.contains("role=\"alert\""), "{}", adults.body);
    let taken = group("взрослые", vec![("access", "all".into())]).await;
    assert!(
        taken.body.contains("id=\"group-settings\"") && taken.body.contains("role=\"alert\""),
        "a taken name comes back in the dialog: {}",
        taken.body
    );
    group(
        "Работа",
        vec![
            ("access", "selected".into()),
            ("categories", work.clone()),
            ("members", ids["Папа"].clone()),
        ],
    )
    .await;
    group(
        "Дети",
        vec![
            ("access", "selected".into()),
            ("categories", kids.clone()),
            ("members", ids["Маша"].clone()),
        ],
    )
    .await;
    let group_id = |name: &'static str| {
        let state = &state;
        async move {
            sqlx::query_scalar::<_, String>("SELECT id FROM user_groups WHERE name = $1")
                .bind(name)
                .fetch_one(&state.db)
                .await
                .unwrap()
        }
    };
    let children = group_id("Дети").await;

    // Masha is 8: of her section only 0+, the unrated book hidden until allowed.
    let masha = |unrated_on: bool| {
        let mut form = vec![
            ("access", "all".to_owned()),
            ("groups", children.clone()),
            ("birth_date", born_years_ago(8)),
        ];
        if unrated_on {
            form.push(("unrated_on", "on".into()));
        }
        settings("Маша", form)
    };
    let saved = masha(false).await;
    assert!(!saved.body.contains("role=\"alert\""), "{}", saved.body);
    let future = settings(
        "Маша",
        vec![
            ("access", "all".into()),
            ("birth_date", "2999-01-01".into()),
        ],
    )
    .await;
    assert!(future.body.contains("role=\"alert\""), "{}", future.body);

    let sign_in = |name: &'static str| {
        let state = &state;
        async move {
            send(state, app_login(login(name), login(name)))
                .await
                .json()["access_token"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let synced = |token: String, cursor: Option<String>| {
        let state = &state;
        async move {
            let uri = cursor.map_or("/v1/sync".to_owned(), |c| format!("/v1/sync?cursor={c}"));
            send(state, bearer_get(&uri, &token)).await.json()
        }
    };
    let ids_of = |page: &serde_json::Value| {
        let mut ids: Vec<String> = page["books"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort_unstable();
        ids
    };
    let sorted = |books: &[&String]| {
        let mut books: Vec<String> = books.iter().map(|b| (*b).clone()).collect();
        books.sort_unstable();
        books
    };

    let masha_token = sign_in("Маша").await;
    let page = synced(masha_token.clone(), None).await;
    assert_eq!(ids_of(&page), sorted(&[&tale]));
    let stream = send(
        &state,
        bearer_get(&format!("/v1/books/{teen}/files/0/stream"), &masha_token),
    )
    .await;
    assert_eq!(stream.problem(), "not_found");
    masha(true).await;
    let page = synced(masha_token.clone(), None).await;
    assert_eq!(ids_of(&page), sorted(&[&tale, &unrated]));

    // A birthday that lifts the ceiling: the saved one is older, so the next sync is full.
    sqlx::query("UPDATE access_policies SET max_rating = 0 WHERE user_id = $1")
        .bind(&ids["Маша"])
        .execute(&state.db)
        .await
        .unwrap();
    let cursor = page["cursor"].as_str().unwrap().to_owned();
    let next = synced(masha_token.clone(), Some(cursor)).await;
    assert_eq!(next["reset"], true, "{next}");
    let ceiling: Option<i64> =
        sqlx::query_scalar("SELECT max_rating FROM access_policies WHERE user_id = $1")
            .bind(&ids["Маша"])
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(ceiling, Some(6));

    // Groups add up: Papa is in Adults and Work, Mama only in Adults.
    let papa = synced(sign_in("Папа").await, None).await;
    assert_eq!(
        ids_of(&papa),
        sorted(&[&tale, &teen, &unrated, &novel, &report])
    );
    let mama_token = sign_in("Мама").await;
    let mama = synced(mama_token.clone(), None).await;
    assert_eq!(ids_of(&mama), sorted(&[&tale, &teen, &unrated, &novel]));

    // The reviewer outside groups keeps their own pick.
    let review = synced(sign_in("Модерация").await, None).await;
    assert_eq!(ids_of(&review), sorted(&[&novel]));

    // Any change of a group: phones of everyone sync in full.
    let adults_id = group_id("Взрослые").await;
    let resaved = send(
        &state,
        post(
            &format!("/groups/{adults_id}/settings"),
            Some(&session),
            &[
                ("name", "Взрослые"),
                ("access", "all"),
                ("members", &format!("{},{}", ids["Мама"], ids["Папа"])),
            ],
        ),
    )
    .await;
    assert!(!resaved.body.contains("role=\"alert\""), "{}", resaved.body);
    let cursor = mama["cursor"].as_str().unwrap().to_owned();
    assert_eq!(
        synced(mama_token.clone(), Some(cursor)).await["reset"],
        true
    );

    // The users page shows groups and the child's ceiling; the section dialog its group.
    let page = send(&state, get("/users", Some(&session))).await;
    for text in ["Взрослые", "Работа", "Дети", "up to 6+"] {
        assert!(page.body.contains(text), "{text}: {}", page.body);
    }
    let tab = send(&state, get("/users/groups", Some(&session))).await;
    assert!(
        tab.body.contains(&format!("/groups/{children}/settings"))
            && tab.body.contains("Маша")
            && !tab.body.contains("Модерация"),
        "the groups tab lists groups with their members: {}",
        tab.body
    );
    let edit = send(
        &state,
        get(&format!("/categories/{work}/edit"), Some(&session)),
    )
    .await;
    assert!(edit.body.contains("Работа"), "{}", edit.body);

    // A deleted group: Papa loses the closed section, Mama's view does not change.
    let work_group = group_id("Работа").await;
    send(
        &state,
        post(&format!("/groups/{work_group}/delete"), Some(&session), &[]),
    )
    .await;
    let papa = synced(sign_in("Папа").await, None).await;
    assert_eq!(ids_of(&papa), sorted(&[&tale, &teen, &unrated, &novel]));
    let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_members WHERE group_id = $1")
        .bind(&work_group)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(members, 0);
}
