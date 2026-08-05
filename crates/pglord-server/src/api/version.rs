use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::ClusterOps;

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
}

#[utoipa::path(
    get,
    path = "/api/v1/version",
    tag = "system",
    responses((status = 200, description = "Server version", body = VersionResponse))
)]
pub async fn version(State(ops): State<Option<ClusterOps>>) -> Json<VersionResponse> {
    Json(VersionResponse {
        version: pglord_core::VERSION.into(),
        namespace: ops.as_ref().map(|o| o.ns.clone()).unwrap_or_default(),
        all_namespaces: ops.as_ref().is_some_and(|o| o.all_namespaces),
    })
}
