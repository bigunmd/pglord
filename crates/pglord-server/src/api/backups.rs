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
    /// Cron schedule, or null when backups are taken manually only.
    pub schedule: Option<String>,
    /// What is in the repository — this is what could be restored.
    pub backups: Vec<BackupEntry>,
    /// What happened recently, including running and failed attempts.
    pub runs: Vec<crate::backup_runs::BackupRun>,
}

#[derive(Deserialize, ToSchema, Default)]
pub struct BackupRequest {
    /// "full" (default), "diff" or "incr".
    #[serde(default)]
    pub kind: Option<pglord_core::backup::BackupKind>,
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
            schedule: None,
            backups: Vec::new(),
            runs: Vec::new(),
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
    let runs = crate::backup_runs::stale_running_to_unknown(
        crate::backup_runs::list(&ops.kube, &ops.ns, &name).await,
        now_seconds(),
    );
    Ok(Json(BackupList {
        repository: backup::repository_uri(&spec.backup),
        enabled: true,
        schedule: spec.backup.schedule.clone(),
        backups,
        runs,
    }))
}

pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
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
    body: Option<Json<BackupRequest>>,
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
    let runs = crate::backup_runs::stale_running_to_unknown(
        crate::backup_runs::list(&ops.kube, &ops.ns, &name).await,
        now_seconds(),
    );
    if crate::backup_runs::has_run_in_flight(&runs) {
        return Err(ApiError::conflict("backup_in_progress")
            .with_detail("A backup of this cluster is already running."));
    }
    let kind = body.and_then(|Json(b)| b.kind).unwrap_or_default();
    start_backup(ops, &spec, kind, "manual")
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::ACCEPTED)
}

/// Record a run and fire the backup on the primary, detached. Shared by the
/// API and the scheduler so both produce identical history.
pub async fn start_backup(
    ops: &ClusterOps,
    spec: &pglord_core::spec::ClusterSpec,
    kind: pglord_core::backup::BackupKind,
    trigger: &str,
) -> anyhow::Result<String> {
    let name = spec.name.clone();
    let primary = ops
        .store
        .primary(&name)
        .await?
        .unwrap_or_else(|| format!("{name}-0"));
    let started = now_seconds();
    let id = format!("{started}-{}", kind.as_str());

    crate::backup_runs::record_start(
        &ops.kube,
        &ops.ns,
        &name,
        crate::backup_runs::BackupRun {
            id: id.clone(),
            kind: kind.as_str().to_string(),
            trigger: trigger.to_string(),
            state: crate::backup_runs::RunState::Running,
            started,
            finished: None,
            error: None,
        },
    )
    .await?;

    // Backups take minutes; run detached and let the run record carry the
    // outcome rather than holding a request open.
    let url = format!(
        "http://{}:{}/backup",
        pod_fqdn(&primary, &name, &ops.ns),
        AGENT_HTTP_PORT
    );
    let http = ops.http.clone();
    let kube = ops.kube.clone();
    let ns = ops.ns.clone();
    let run_id = id.clone();
    tokio::spawn(async move {
        tracing::info!(cluster = %name, %primary, kind = kind.as_str(), run = %run_id, "backup started");
        let result = http
            .post(&url)
            .json(&serde_json::json!({ "kind": kind.as_str() }))
            .timeout(std::time::Duration::from_secs(3600))
            .send()
            .await;
        let (state, error) = match result {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(cluster = %name, "backup finished");
                (crate::backup_runs::RunState::Succeeded, None)
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(cluster = %name, %status, %body, "backup failed");
                (
                    crate::backup_runs::RunState::Failed,
                    Some(summarize_error(&body, status.as_u16())),
                )
            }
            Err(err) => {
                tracing::warn!(cluster = %name, %err, "backup request failed");
                (crate::backup_runs::RunState::Failed, Some(err.to_string()))
            }
        };
        if let Err(err) = crate::backup_runs::record_finish(
            &kube,
            &ns,
            &name,
            &run_id,
            state,
            now_seconds(),
            error,
        )
        .await
        {
            tracing::warn!(cluster = %name, %err, "could not record backup outcome");
        }
    });
    Ok(id)
}

/// Keep the agent's message, trimmed to something a UI row can show.
fn summarize_error(body: &str, status: u16) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.trim().to_string());
    let message = if message.is_empty() {
        format!("agent returned HTTP {status}")
    } else {
        message
    };
    message.chars().take(500).collect()
}
