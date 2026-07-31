use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use pglord_core::backup;
use pglord_core::render::{AGENT_HTTP_PORT, pod_fqdn};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::app::ClusterOps;
use crate::error::ApiError;

#[derive(Serialize, Deserialize, ToSchema)]
pub struct BackupEntry {
    /// pgBackRest label, e.g. "20260731-120000F".
    pub label: String,
    /// "full", "diff" or "incr".
    pub kind: String,
    /// Unix seconds.
    pub started: i64,
    pub finished: i64,
    /// Bytes this backup occupies in the repository.
    pub size_bytes: i64,
    pub wal_start: Option<String>,
    pub wal_end: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct BackupList {
    /// Where backups are stored, or null when backups are off.
    pub repository: Option<String>,
    pub enabled: bool,
    pub backups: Vec<BackupEntry>,
}

#[derive(Deserialize)]
struct AgentBackups {
    backups: Vec<BackupEntry>,
}

fn ops(state: &Option<ClusterOps>) -> Result<&ClusterOps, ApiError> {
    state.as_ref().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "kube_unavailable")
            .with_detail("No Kubernetes environment available; backups are unavailable.")
    })
}

/// List the backups in a cluster's repository.
#[utoipa::path(
    get,
    path = "/api/v1/clusters/{name}/backups",
    tag = "backups",
    params(("name" = String, Path, description = "Cluster name")),
    responses(
        (status = 200, description = "Backups in the repository", body = BackupList),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn list_backups(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
) -> Result<Json<BackupList>, ApiError> {
    let ops = ops(&state)?;
    let Some((spec, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };
    if !spec.backup.enabled {
        return Ok(Json(BackupList {
            repository: None,
            enabled: false,
            backups: Vec::new(),
        }));
    }
    let primary = ops
        .store
        .primary(&name)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| format!("{name}-0"));
    let url = format!(
        "http://{}:{}/backups",
        pod_fqdn(&primary, &spec.name, &ops.ns),
        AGENT_HTTP_PORT
    );
    // A repository that cannot be read right now is reported as empty rather
    // than as a failure: the cluster itself is fine.
    let backups = match ops.http.get(&url).send().await {
        Ok(resp) => resp
            .json::<AgentBackups>()
            .await
            .map(|b| b.backups)
            .unwrap_or_default(),
        Err(err) => {
            tracing::warn!(cluster = %name, %err, "could not read backup list from primary");
            Vec::new()
        }
    };
    Ok(Json(BackupList {
        repository: backup::repository_uri(&spec.backup),
        enabled: true,
        backups,
    }))
}

/// Take a full backup now, on the primary.
#[utoipa::path(
    post,
    path = "/api/v1/clusters/{name}/backups",
    tag = "backups",
    params(("name" = String, Path, description = "Cluster name")),
    responses(
        (status = 202, description = "Backup started"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "Backups are not configured", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn create_backup(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let Some((spec, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };
    if !spec.backup.enabled {
        return Err(ApiError::conflict("backups_disabled").with_detail(
            "Configure a backup repository for this cluster before taking a backup.",
        ));
    }
    let primary = ops
        .store
        .primary(&name)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| format!("{name}-0"));

    // Backups take minutes; run it detached and let the list reflect the
    // result rather than holding the request open.
    let url = format!(
        "http://{}:{}/backup",
        pod_fqdn(&primary, &spec.name, &ops.ns),
        AGENT_HTTP_PORT
    );
    let http = ops.http.clone();
    let cluster = name.clone();
    tokio::spawn(async move {
        tracing::info!(%cluster, %primary, "backup started");
        match http
            .post(&url)
            .timeout(std::time::Duration::from_secs(3600))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(%cluster, "backup finished")
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(%cluster, %status, %body, "backup failed");
            }
            Err(err) => tracing::warn!(%cluster, %err, "backup request failed"),
        }
    });
    Ok(StatusCode::ACCEPTED)
}
