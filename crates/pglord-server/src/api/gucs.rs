use axum::Json;
use pglord_core::guc;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct GucInfo {
    pub name: String,
    /// Value shape: integer, real, bool, memory, duration, enum or stringlike.
    pub kind: String,
    /// "reload" (applies live) or "restart".
    pub apply: String,
    pub category: String,
    pub summary: String,
    pub default: String,
}

/// The settings pglord lets you tune, with the metadata a config editor needs.
#[utoipa::path(
    get,
    path = "/api/v1/gucs",
    tag = "system",
    responses((status = 200, description = "Tunable settings", body = [GucInfo]))
)]
pub async fn list_gucs() -> Json<Vec<GucInfo>> {
    Json(
        guc::catalog()
            .iter()
            .map(|g| GucInfo {
                name: g.name.to_string(),
                kind: format!("{:?}", g.kind).to_lowercase(),
                apply: format!("{:?}", g.apply).to_lowercase(),
                category: g.category.to_string(),
                summary: g.summary.to_string(),
                default: g.default.to_string(),
            })
            .collect(),
    )
}
