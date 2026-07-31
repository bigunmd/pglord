use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct VersionResponse {
    pub version: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/version",
    tag = "system",
    responses((status = 200, description = "Server version", body = VersionResponse))
)]
pub async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        version: pglord_core::VERSION.into(),
    })
}
