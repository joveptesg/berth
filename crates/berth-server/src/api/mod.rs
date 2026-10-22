//! JSON API v1.

mod server_info;

use axum::Router;
use berth_core::BerthError;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{AppState, error::ApiError};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Berth API",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    components(schemas(crate::error::Problem))
)]
struct ApiDoc;

fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi()).routes(routes!(server_info::server_info))
}

/// HTTP router of the server.
pub fn router(state: AppState) -> Router {
    let (router, _) = routes().split_for_parts();
    router
        .merge(crate::admin::router(state.clone()))
        .fallback(async || ApiError::from(BerthError::NotFound))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// `OpenAPI` specification of the API.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    routes().into_openapi()
}
