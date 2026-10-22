//! JSON API v1.

mod admin;
mod auth;
mod me;
mod server_info;

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::{
    Router,
    http::{Method, header},
    response::IntoResponse,
};
use berth_core::BerthError;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};
use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{AppState, Config, error::ApiError};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Berth API",
        license(name = "Apache-2.0", identifier = "Apache-2.0")
    ),
    modifiers(&BearerAuth),
    security(("bearer" = [])),
    components(schemas(crate::error::Problem))
)]
struct ApiDoc;

/// Access tokens from pairing or refresh go into `Authorization: Bearer`.
struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi
            .components
            .get_or_insert_default()
            .add_security_scheme(
                "bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT")
                        .build(),
                ),
            );
    }
}

fn routes() -> OpenApiRouter<AppState> {
    // Pairing codes have about 40 bits: guessing is slowed per client address.
    let pair_limit = GovernorLayer::new(Arc::new(
        GovernorConfigBuilder::default()
            .per_second(3)
            .burst_size(10)
            .finish()
            .expect("valid governor config"),
    ))
    .error_handler(|_| ApiError::from(BerthError::RateLimited).into_response());

    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(server_info::server_info))
        .merge(
            OpenApiRouter::new()
                .routes(routes!(auth::pair))
                .layer(pair_limit),
        )
        .routes(routes!(auth::refresh))
        .routes(routes!(auth::revoke))
        .routes(routes!(me::me))
        .routes(routes!(admin::create_pairing_token))
}

/// CORS for browser clients of the API (a web build of the app): origins from
/// `cors_origins`, plus any `localhost` origin in debug builds. The admin needs none.
fn cors(config: Arc<ArcSwap<Config>>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            let Ok(origin) = origin.to_str() else {
                return false;
            };
            let local = url::Url::parse(origin).is_ok_and(|url| {
                matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
            });
            (cfg!(debug_assertions) && local)
                || config
                    .load()
                    .cors_origins
                    .iter()
                    .any(|allowed| allowed.trim_end_matches('/') == origin)
        }))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
}

/// HTTP router of the server.
pub fn router(state: AppState) -> Router {
    let (api, _) = routes().split_for_parts();
    api.layer(cors(state.config.clone()))
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
