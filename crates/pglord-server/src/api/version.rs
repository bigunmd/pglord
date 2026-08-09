use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::AppState;

#[derive(Serialize, ToSchema)]
pub struct VersionResponse {
    pub version: String,
    /// The namespace this server manages. Clients that have no namespace
    /// context of their own — creating the very first cluster, for instance —
    /// must target this one rather than guessing "default".
    pub namespace: String,
    /// The server watches every namespace, so `namespace` is only its own
    /// home and clusters may live anywhere.
    pub all_namespaces: bool,
    /// The image a cluster gets when it names none — which is what decides
    /// its PostgreSQL major. Reported so the UI can show it instead of
    /// leaving the most consequential default invisible.
    pub default_agent_image: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/version",
    tag = "system",
    responses((status = 200, description = "Server version", body = VersionResponse))
)]
pub async fn version(State(state): State<AppState>) -> Json<VersionResponse> {
    let ops = state.cluster_ops.as_ref();
    Json(VersionResponse {
        version: pglord_core::VERSION.into(),
        namespace: ops.map(|o| o.ns.clone()).unwrap_or_default(),
        all_namespaces: ops.is_some_and(|o| o.all_namespaces),
        // From config, not ClusterOps: it is the configured answer whether or
        // not a Kubernetes environment exists.
        default_agent_image: state.cfg.default_agent_image.clone(),
    })
}
