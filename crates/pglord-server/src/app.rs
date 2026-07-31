//! The single composition seam: builds the full router and the OpenAPI doc.
//! Called from three places — main(), --dump-openapi, and integration tests —
//! so wiring can never drift between runtime, spec, and tests.

use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::cors::{AllowOrigin, CorsLayer};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

use crate::config::Config;
use crate::health::{self, Readiness};
use crate::spec_store::SpecStore;

#[derive(OpenApi)]
#[openapi(info(
    title = "pglord API",
    description = "Control plane for HA PostgreSQL clusters on Kubernetes."
))]
struct ApiDoc;

/// Everything cluster management needs; absent when no Kubernetes environment
/// is available (handlers then answer 503 kube_unavailable).
#[derive(Clone)]
pub struct ClusterOps {
    pub store: Arc<dyn SpecStore>,
    pub kube: kube::Client,
    pub http: reqwest::Client,
    pub ns: String,
}

#[derive(Clone)]
pub struct AppState {
    pub readiness: Readiness,
    pub cfg: Arc<Config>,
    pub cluster_ops: Option<ClusterOps>,
}

impl FromRef<AppState> for Readiness {
    fn from_ref(s: &AppState) -> Readiness {
        s.readiness.clone()
    }
}

impl FromRef<AppState> for Option<ClusterOps> {
    fn from_ref(s: &AppState) -> Option<ClusterOps> {
        s.cluster_ops.clone()
    }
}

pub fn build(
    readiness: Readiness,
    cfg: &Config,
    cluster_ops: Option<ClusterOps>,
) -> (Router, utoipa::openapi::OpenApi) {
    let api = OpenApiRouter::new()
        .routes(routes!(crate::api::ping::ping))
        .routes(routes!(crate::api::version::version))
        .routes(routes!(
            crate::api::clusters::create_cluster,
            crate::api::clusters::list_clusters
        ))
        .routes(routes!(
            crate::api::clusters::get_cluster,
            crate::api::clusters::delete_cluster,
            crate::api::clusters::patch_cluster
        ))
        .routes(routes!(crate::api::clusters::switchover))
        .routes(routes!(crate::api::preflight::get_preflight));

    let (api_router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(api)
        .split_for_parts();

    // Router-wide fallback: unmatched /api paths keep the problem+json shape;
    // anything else is the SPA (embedded assets with index.html fallback).
    let api_router = api_router.fallback(async |uri: axum::http::Uri| {
        if uri.path().starts_with("/api") {
            crate::error::ApiError::not_found().into_response()
        } else {
            crate::ui_assets::serve(uri.path())
        }
    });

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
        cluster_ops,
    };
    (router.with_state(state), api)
}
