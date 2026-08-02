use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use k8s_openapi::api::apps::v1::StatefulSet;
use kube::api::Api;
use pglord_core::render::{AGENT_HTTP_PORT, pod_fqdn};
use pglord_core::spec::{ClusterSpec, SpecChange, SpecError};
use serde::{Deserialize, Serialize};
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
    /// Settings are staged on this instance that only a restart will apply.
    pub pending_restart: bool,
    /// Primary only: connected standbys eligible to confirm a synchronous
    /// commit. Null on standbys and unreachable instances.
    pub sync_standby_count: Option<u32>,
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
    /// PostgreSQL settings applied on top of the defaults.
    pub parameters: std::collections::BTreeMap<String, String>,
    /// Client access rules between pglord's managed prefix and catch-all.
    pub pg_hba: Vec<pglord_core::hba::HbaRule>,
    /// Scheduling, resources and Kubernetes metadata for the instances.
    pub workload: pglord_core::workload::WorkloadSpec,
    /// Backup repository (secret name only — never its contents).
    pub backup: pglord_core::backup::BackupSpec,
    /// Where this cluster's data came from, when it was created by a restore.
    /// Null for clusters that started empty.
    pub restore: Option<pglord_core::restore::RestoreSpec>,
    /// Secret the passwords come from when the user supplied their own; null
    /// when pglord generates them.
    pub credentials_secret: Option<String>,
    /// Custom environment variables on the instances (secret *references*
    /// only — never their values).
    pub env: Vec<pglord_core::env::EnvVarSpec>,
    /// Sources of the one-time initialisation scripts.
    pub init: pglord_core::init::InitSpec,
    /// Any instance is waiting on a restart to apply staged settings.
    pub pending_restart: bool,
    /// Commits wait for any one standby to confirm the WAL flush.
    pub synchronous: bool,
    /// Synchronous replication is on but the primary sees no connected sync
    /// standby: commits are blocking until one reconnects.
    pub sync_degraded: bool,
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
    use super::{role_of, roll_order};

    #[test]
    fn role_mapping_matches_recovery_state() {
        assert_eq!(role_of(Some(false)), "primary");
        assert_eq!(role_of(Some(true)), "standby");
        assert_eq!(role_of(None), "unknown");
    }

    #[test]
    fn roll_order_is_standbys_first_primary_last() {
        assert_eq!(
            roll_order("demo", 3, "demo-1"),
            vec!["demo-0", "demo-2", "demo-1"]
        );
        assert_eq!(roll_order("demo", 1, "demo-0"), vec!["demo-0"]);
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
    Json(body): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<ClusterSpec>), ApiError> {
    let ops = ops(&state)?;
    // Taken as raw JSON purely to tell "no image given" from "this exact
    // image". The spec type carries a compile-time default, which cannot know
    // what image this deployment can actually pull.
    let image_omitted = body.get("image").is_none_or(serde_json::Value::is_null);
    let mut spec: ClusterSpec = serde_json::from_value(body).map_err(|e| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_spec").with_detail(e.to_string())
    })?;
    if image_omitted {
        spec.image.clone_from(&ops.default_agent_image);
    }
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
    let pending_restart = observations.iter().any(|o| o.pending_restart);
    let sync_degraded = spec.synchronous
        && observations
            .iter()
            .any(|o| o.pod == primary && o.ready && o.sync_standby_count == Some(0));
    let instances = observations
        .into_iter()
        .map(|o| InstanceDto {
            role: role_of(o.in_recovery).to_string(),
            pod: o.pod,
            ready: o.ready,
            lsn: o.lsn,
            pending_restart: o.pending_restart,
            sync_standby_count: o.sync_standby_count,
        })
        .collect();
    Ok(Json(ClusterDetail {
        name: spec.name,
        image: spec.image,
        replicas: spec.replicas,
        storage_size: spec.storage_size,
        ready_replicas,
        primary,
        parameters: spec.parameters,
        pg_hba: spec.pg_hba,
        workload: spec.workload,
        backup: spec.backup,
        restore: spec.restore,
        credentials_secret: spec.credentials_secret,
        env: spec.env,
        init: spec.init,
        pending_restart,
        synchronous: spec.synchronous,
        sync_degraded,
        instances,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct SwitchoverRequest {
    /// Pod to promote, e.g. "demo-1".
    pub target: String,
}

fn spec_error(e: SpecError) -> ApiError {
    let code = match e {
        SpecError::StorageShrink { .. } => "storage_shrink",
        SpecError::UnknownParameter(_)
        | SpecError::ReservedParameter(_)
        | SpecError::InvalidParameterValue { .. } => "invalid_parameter",
        SpecError::SyncRequiresReplicas => "sync_requires_replicas",
        SpecError::SyncConflictsSynchronousCommit(_) => "sync_conflicts_synchronous_commit",
        SpecError::InvalidHba(_) => "invalid_pg_hba",
        SpecError::InvalidWorkload(_) => "invalid_workload",
        SpecError::InvalidBackup(_) => "invalid_backup",
        _ => "invalid_spec",
    };
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code).with_detail(e.to_string())
}

/// Change a running cluster: scale instances or grow storage.
#[utoipa::path(
    patch,
    path = "/api/v1/clusters/{name}",
    tag = "clusters",
    params(("name" = String, Path, description = "Cluster name")),
    request_body = SpecChange,
    responses(
        (status = 200, description = "Updated cluster", body = ClusterDetail),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 422, description = "Rejected change", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn patch_cluster(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
    Json(change): Json<SpecChange>,
) -> Result<Json<ClusterDetail>, ApiError> {
    let ops = ops(&state)?;
    let Some((current, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };
    let next = current.apply_change(&change).map_err(spec_error)?;

    // Grow volumes before the spec lands so a rejected expansion (StorageClass
    // without allowVolumeExpansion) leaves the cluster exactly as it was.
    if next.storage_size != current.storage_size {
        crate::reconciler::expand_pvcs(
            &ops.kube,
            &ops.ns,
            &next.name,
            current.replicas,
            &next.storage_size,
        )
        .await
        .map_err(|e| {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "storage_expansion_failed")
                .with_detail(format!(
                    "Kubernetes rejected the volume expansion: {e}. The StorageClass must allow volume expansion."
                ))
        })?;
    }

    ops.store.put(&next).await.map_err(ApiError::internal)?;

    if next.replicas < current.replicas {
        crate::reconciler::delete_orphan_pvcs(
            &ops.kube,
            &ops.ns,
            &next.name,
            next.replicas,
            current.replicas,
        )
        .await;
    }
    tracing::info!(cluster = %next.name, replicas = next.replicas, storage = %next.storage_size, "cluster updated");
    get_cluster(State(state.clone()), Path(name)).await
}

/// Promote a specific standby (planned switchover).
#[utoipa::path(
    post,
    path = "/api/v1/clusters/{name}/switchover",
    tag = "clusters",
    params(("name" = String, Path, description = "Cluster name")),
    request_body = SwitchoverRequest,
    responses(
        (status = 202, description = "Switchover started"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "Target cannot be promoted", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 422, description = "Target is not part of this cluster", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn switchover(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
    Json(req): Json<SwitchoverRequest>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let Some((spec, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };

    let valid_pods: Vec<String> = (0..spec.replicas).map(|i| format!("{name}-{i}")).collect();
    if !valid_pods.contains(&req.target) {
        return Err(
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_target").with_detail(format!(
                "'{}' is not an instance of this cluster.",
                req.target
            )),
        );
    }

    let current = ops
        .store
        .primary(&name)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| format!("{name}-0"));
    if current == req.target {
        return Err(ApiError::conflict("already_primary")
            .with_detail("That instance is already the primary."));
    }

    // Only a ready standby may be promoted: promoting an instance that is not
    // streaming would silently lose whatever WAL it never received.
    let observations =
        crate::instance_probe::observe_cluster(&ops.http, &spec.name, &ops.ns, spec.replicas).await;
    let target_ok = observations
        .iter()
        .find(|o| o.pod == req.target)
        .map(|o| o.ready && o.in_recovery == Some(true))
        .unwrap_or(false);
    if !target_ok {
        return Err(ApiError::conflict("target_not_ready").with_detail(
            "The target instance is not a ready standby; wait for it to catch up and retry.",
        ));
    }

    if !ops
        .store
        .set_primary(&name, &req.target)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::conflict("record_changed")
            .with_detail("The primary record changed concurrently; retry."));
    }
    tracing::warn!(cluster = %name, from = %current, to = %req.target, "SWITCHOVER: primary record updated");
    ops.metrics
        .failover_total
        .with_label_values(&[name.as_str(), "manual"])
        .inc();

    // The target also self-promotes by following the record; this just makes it
    // immediate.
    let url = format!(
        "http://{}:{}/promote",
        pod_fqdn(&req.target, &spec.name, &ops.ns),
        AGENT_HTTP_PORT
    );
    if let Err(err) = ops.http.post(&url).send().await {
        tracing::debug!(%err, "promote POST failed (agent will self-promote)");
    }
    Ok(StatusCode::ACCEPTED)
}

/// Restart every instance so staged settings take effect. Standbys go first
/// and the primary last, keeping writes available for as long as possible.
#[utoipa::path(
    post,
    path = "/api/v1/clusters/{name}/restart",
    tag = "clusters",
    params(("name" = String, Path, description = "Cluster name")),
    responses(
        (status = 202, description = "Rolling restart started"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn restart_cluster(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
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

    let ops = ops.clone();
    tokio::spawn(async move {
        let order = roll_order(&spec.name, spec.replicas, &primary);
        tracing::info!(cluster = %spec.name, ?order, "rolling restart started");
        roll_pods(&ops, &spec, order).await;
        tracing::info!(cluster = %spec.name, "rolling restart finished");
    });
    Ok(StatusCode::ACCEPTED)
}

/// Standbys first, primary last: writes stay available for as long as possible.
pub(crate) fn roll_order(cluster: &str, replicas: u8, primary: &str) -> Vec<String> {
    let mut order: Vec<String> = (0..replicas)
        .map(|i| format!("{cluster}-{i}"))
        .filter(|p| p != primary)
        .collect();
    order.push(primary.to_string());
    order
}

/// Delete each pod in order, waiting for it to come back ready before the
/// next, so the cluster never loses more than one instance at a time.
pub(crate) async fn roll_pods(
    ops: &ClusterOps,
    spec: &pglord_core::spec::ClusterSpec,
    order: Vec<String>,
) {
    let pods: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(ops.kube.clone(), &ops.ns);
    for pod in order {
        if let Err(err) = pods.delete(&pod, &Default::default()).await {
            tracing::warn!(%pod, %err, "roll: pod delete failed");
            continue;
        }
        for _ in 0..60u32 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let obs = crate::instance_probe::observe_cluster(
                &ops.http,
                &spec.name,
                &ops.ns,
                spec.replicas,
            )
            .await;
            if obs.iter().any(|o| o.pod == pod && o.ready) {
                break;
            }
        }
        tracing::info!(%pod, "roll: instance back");
    }
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
