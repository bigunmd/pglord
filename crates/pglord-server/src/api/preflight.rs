use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::ClusterOps;
use crate::error::ApiError;
use crate::preflight;

#[derive(Serialize, ToSchema)]
pub struct PreflightCheck {
    /// Resource name, group-qualified when not core (e.g. "statefulsets.apps").
    pub resource: String,
    pub verb: String,
    /// null when the check itself could not be performed.
    pub allowed: Option<bool>,
    pub reason: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct PreflightReport {
    pub namespace: String,
    /// True only when every required permission is granted.
    pub ok: bool,
    /// Permissions denied or unverifiable.
    pub missing: i32,
    pub checks: Vec<PreflightCheck>,
}

/// Report which cluster permissions pglord actually has, straight from the
/// apiserver's own authorization decisions.
#[utoipa::path(
    get,
    path = "/api/v1/preflight",
    tag = "system",
    responses(
        (status = 200, description = "Permission report", body = PreflightReport),
        (status = 503, description = "No Kubernetes environment", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn get_preflight(
    State(state): State<Option<ClusterOps>>,
) -> Result<Json<PreflightReport>, ApiError> {
    let ops = state.as_ref().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "kube_unavailable")
            .with_detail("No Kubernetes environment available; permissions cannot be checked.")
    })?;
    let results = preflight::run(&ops.kube, &ops.ns, &ops.spec_backend).await;
    let (ok, missing) = preflight::summarize(&results);
    Ok(Json(PreflightReport {
        namespace: ops.ns.clone(),
        ok,
        missing: missing as i32,
        checks: results
            .into_iter()
            .map(|r| PreflightCheck {
                resource: r.resource,
                verb: r.verb,
                allowed: r.allowed,
                reason: r.reason,
            })
            .collect(),
    }))
}
