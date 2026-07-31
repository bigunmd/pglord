use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct PingResponse {
    pub message: String,
}

/// Unauthenticated liveness of the API surface (distinct from /healthz: this
/// one is part of the versioned contract and appears in the OpenAPI spec).
#[utoipa::path(
    get,
    path = "/api/v1/ping",
    tag = "system",
    responses((status = 200, description = "Service reachable", body = PingResponse))
)]
pub async fn ping() -> Json<PingResponse> {
    Json(PingResponse {
        message: "pong".into(),
    })
}
