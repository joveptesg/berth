//! Integration tests: first start and `GET /v1/server-info`.

use std::path::Path;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use berth_server::AppState;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn start(dir: &Path) -> AppState {
    let config = dir.join("berth.toml");
    let data_dir = dir.join("data");
    std::fs::write(
        &config,
        format!(
            "data_dir = '{}'\nname = 'Test library'\n",
            data_dir.display()
        ),
    )
    .unwrap();
    berth_server::bootstrap(Some(&config)).await.unwrap()
}

async fn get(state: AppState, uri: &str) -> (StatusCode, Option<String>, Value) {
    let response = berth_server::router(state)
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, content_type, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn server_info_is_public() {
    let dir = tempfile::tempdir().unwrap();
    let state = start(dir.path()).await;
    let server_id = state.server_id.to_string();

    let (status, _, body) = get(state, "/v1/server-info").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Test library");
    assert_eq!(body["server_id"], server_id.as_str());
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["auth_modes"], serde_json::json!(["pairing"]));
    assert!(body["min_client_version"]["mobile"].is_string());
}

#[tokio::test]
async fn first_start_creates_data_dir_and_keeps_server_id() {
    let dir = tempfile::tempdir().unwrap();
    let first = start(dir.path()).await;
    let data_dir = dir.path().join("data");
    assert!(data_dir.join("berth.sqlite").is_file());
    assert!(data_dir.join("secret.key").is_file());
    first.db.close().await;

    let second = start(dir.path()).await;
    assert_eq!(first.server_id, second.server_id);
}

#[tokio::test]
async fn unknown_route_returns_problem_json() {
    let dir = tempfile::tempdir().unwrap();
    let (status, content_type, body) = get(start(dir.path()).await, "/v1/nope").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    assert_eq!(body["code"], "not_found");
    assert_eq!(body["status"], 404);
}

#[test]
fn openapi_documents_server_info() {
    let spec = serde_json::to_value(berth_server::openapi()).unwrap();
    assert!(spec["paths"]["/v1/server-info"]["get"].is_object());
    assert!(spec["components"]["schemas"]["Problem"].is_object());
}
