use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use k8s_openapi::api::apps::v1::StatefulSet;
use kube::api::Api;
use pglord_core::spec::ClusterSpec;
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::ClusterOps;
use crate::error::ApiError;

#[derive(Serialize, ToSchema)]
pub struct ClusterSummary {
    pub name: String,
    pub image: String,
    pub replicas: u8,
    /// Instances currently passing their readiness probe.
    pub ready_replicas: i32,
}

#[derive(Serialize, ToSchema)]
pub struct InstanceDto {
    /// Pod name, e.g. "demo-0".
    pub pod: String,
    /// "primary" | "standby" | "unknown" (agent unreachable).
    pub role: String,
    pub ready: bool,
    /// Current/receive WAL LSN as reported by the agent.
    pub lsn: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ClusterDetail {
    pub name: String,
    pub image: String,
    pub replicas: u8,
    pub storage_size: String,
    pub ready_replicas: i32,
    /// Recorded primary pod (the single writer per the primary record).
    pub primary: String,
    pub instances: Vec<InstanceDto>,
}

/// Map an agent observation to the user-facing role string.
pub fn role_of(in_recovery: Option<bool>) -> &'static str {
    match in_recovery {
        Some(false) => "primary",
        Some(true) => "standby",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::role_of;

    #[test]
    fn role_mapping_matches_recovery_state() {
        assert_eq!(role_of(Some(false)), "primary");
        assert_eq!(role_of(Some(true)), "standby");
        assert_eq!(role_of(None), "unknown");
    }
}

fn ops(state: &Option<ClusterOps>) -> Result<&ClusterOps, ApiError> {
    state.as_ref().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "kube_unavailable")
            .with_detail("No Kubernetes environment available; cluster management is disabled.")
    })
}

async fn summarize(ops: &ClusterOps, spec: ClusterSpec) -> ClusterSummary {
    let sts_api: Api<StatefulSet> = Api::namespaced(ops.kube.clone(), &ops.ns);
    let ready = match sts_api.get_opt(&spec.name).await {
        Ok(Some(sts)) => sts.status.and_then(|s| s.ready_replicas).unwrap_or(0),
        _ => 0,
    };
    ClusterSummary {
        name: spec.name,
        image: spec.image,
        replicas: spec.replicas,
        ready_replicas: ready,
    }
}

/// Create a new Postgres cluster.
#[utoipa::path(
    post,
    path = "/api/v1/clusters",
    tag = "clusters",
    request_body = ClusterSpec,
    responses(
        (status = 201, description = "Cluster spec stored", body = ClusterSpec),
        (status = 409, description = "Cluster already exists", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 422, description = "Invalid spec", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 503, description = "No Kubernetes environment", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn create_cluster(
    State(state): State<Option<ClusterOps>>,
    Json(spec): Json<ClusterSpec>,
) -> Result<(StatusCode, Json<ClusterSpec>), ApiError> {
    let ops = ops(&state)?;
    spec.validate().map_err(|e| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_spec").with_detail(e.to_string())
    })?;
    if ops
        .store
        .get(&spec.name)
        .await
        .map_err(ApiError::internal)?
        .is_some()
    {
        return Err(ApiError::conflict("already_exists")
            .with_detail("A cluster with this name already exists."));
    }
    ops.store.put(&spec).await.map_err(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(spec)))
}

/// List clusters.
#[utoipa::path(
    get,
    path = "/api/v1/clusters",
    tag = "clusters",
    responses(
        (status = 200, description = "Clusters", body = [ClusterSummary]),
        (status = 503, description = "No Kubernetes environment", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn list_clusters(
    State(state): State<Option<ClusterOps>>,
) -> Result<Json<Vec<ClusterSummary>>, ApiError> {
    let ops = ops(&state)?;
    let specs = ops.store.list().await.map_err(ApiError::internal)?;
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        out.push(summarize(ops, spec).await);
    }
    Ok(Json(out))
}

/// Get one cluster with per-instance topology.
#[utoipa::path(
    get,
    path = "/api/v1/clusters/{name}",
    tag = "clusters",
    params(("name" = String, Path, description = "Cluster name")),
    responses(
        (status = 200, description = "Cluster detail", body = ClusterDetail),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn get_cluster(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
) -> Result<Json<ClusterDetail>, ApiError> {
    let ops = ops(&state)?;
    let Some((spec, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };
    let primary = ops
        .store
        .primary(&name)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| format!("{name}-0"));
    let observations =
        crate::instance_probe::observe_cluster(&ops.http, &spec.name, &ops.ns, spec.replicas).await;
    let ready_replicas = observations.iter().filter(|o| o.ready).count() as i32;
    let instances = observations
        .into_iter()
        .map(|o| InstanceDto {
            role: role_of(o.in_recovery).to_string(),
            pod: o.pod,
            ready: o.ready,
            lsn: o.lsn,
        })
        .collect();
    Ok(Json(ClusterDetail {
        name: spec.name,
        image: spec.image,
        replicas: spec.replicas,
        storage_size: spec.storage_size,
        ready_replicas,
        primary,
        instances,
    }))
}

/// Delete a cluster (children are garbage-collected via ownerReferences).
#[utoipa::path(
    delete,
    path = "/api/v1/clusters/{name}",
    tag = "clusters",
    params(("name" = String, Path, description = "Cluster name")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn delete_cluster(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    if ops.store.delete(&name).await.map_err(ApiError::internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}
