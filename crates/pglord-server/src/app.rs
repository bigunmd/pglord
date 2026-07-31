//! The single composition seam: builds the full router and the OpenAPI doc.
//! Called from three places — main(), --dump-openapi, and integration tests —
//! so wiring can never drift between runtime, spec, and tests.

use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use axum::routing::get;
use tower_http::cors::{AllowOrigin, CorsLayer};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

use crate::config::Config;
use crate::health::{self, Readiness};

#[derive(OpenApi)]
#[openapi(info(
    title = "pglord API",
    description = "Control plane for HA PostgreSQL clusters on Kubernetes."
))]
struct ApiDoc;

#[derive(Clone)]
pub struct AppState {
    pub readiness: Readiness,
    pub cfg: Arc<Config>,
}

impl FromRef<AppState> for Readiness {
    fn from_ref(s: &AppState) -> Readiness {
        s.readiness.clone()
    }
}

pub fn build(readiness: Readiness, cfg: &Config) -> (Router, utoipa::openapi::OpenApi) {
    let api = OpenApiRouter::new()
        .routes(routes!(crate::api::ping::ping))
        .routes(routes!(crate::api::version::version));

    let (api_router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(api)
        .split_for_parts();

    // Unmatched /api paths must return the same problem+json shape as handler errors.
    let api_router = api_router.fallback(|| async { crate::error::ApiError::not_found() });

    let api_router = if cfg.cors_origins().is_empty() {
        api_router
    } else {
        api_router.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(cfg.cors_origins()))
                .allow_methods(tower_http::cors::Any)
                .allow_headers([
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::AUTHORIZATION,
                ]),
        )
    };

    // /healthz, /readyz, /swagger-ui are merged AFTER the API router: excluded
    // from the OpenAPI contract and from CORS by construction.
    let router = api_router
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api.clone()));

    let state = AppState {
        readiness,
        cfg: Arc::new(cfg.clone()),
    };
    (router.with_state(state), api)
}
