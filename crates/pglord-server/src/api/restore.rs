//! Restoring a backup into a new cluster.
//!
//! A dedicated endpoint rather than a field on create: the server copies the
//! source cluster's repository configuration itself, so a caller never has to
//! restate endpoint, bucket, prefix and credentials — restating them is how you
//! end up restoring from the wrong place.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use pglord_core::restore::RestoreSpec;
use pglord_core::spec::ClusterSpec;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::app::ClusterOps;
use crate::error::ApiError;

#[derive(Deserialize, ToSchema)]
pub struct RestoreRequest {
    /// Name of the cluster to create. Must not already exist.
    pub target_cluster: String,
    /// Backup to restore, as listed in the repository. Omit for the latest.
    #[serde(default)]
    pub backup_label: Option<String>,
    /// Recover to this instant (RFC3339, UTC) instead of the end of the WAL.
    #[serde(default)]
    pub target_time: Option<String>,
    /// Instances in the new cluster. Defaults to the source's.
    #[serde(default)]
    pub replicas: Option<u8>,
}

fn ops(state: &Option<ClusterOps>) -> Result<&ClusterOps, ApiError> {
    state.as_ref().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "kube_unavailable")
            .with_detail("No Kubernetes environment available; restore is unavailable.")
    })
}

/// Build the new cluster's spec from the source. Pure, so the rules that matter
/// — what is inherited and what is deliberately not — are unit-tested.
pub fn restored_spec(source: &ClusterSpec, req: &RestoreRequest) -> ClusterSpec {
    ClusterSpec {
        name: req.target_cluster.trim().to_string(),
        replicas: req.replicas.unwrap_or(source.replicas),
        // Same image and shape as the source: restoring into a differently
        // sized or differently versioned cluster is a separate decision, and a
        // silent one here would be a nasty surprise.
        image: source.image.clone(),
        storage_size: source.storage_size.clone(),
        parameters: source.parameters.clone(),
        // Same data, same audience: the rules that guarded the source guard
        // its restored copy.
        pg_hba: source.pg_hba.clone(),
        workload: source.workload.clone(),
        // Carried over: whatever ran against the source's data expects these.
        env: source.env.clone(),
        // Not carried over. Init scripts run when a data directory is created
        // empty; this one arrives full. Copying the reference would also make
        // the new cluster unschedulable if the source's script ConfigMap has
        // since been deleted, for no benefit.
        init: Default::default(),
        // A restored cluster generates its own credentials. Sharing the
        // source's Secret would mean rotating one silently rotates the other.
        credentials_secret: None,
        // Backups OFF. Inheriting the source's repository would point a second
        // cluster at the source's stanza; the two have different system
        // identifiers, so pgBackRest rejects it and the restored cluster would
        // start life with a broken backup configuration. Enabling backups here
        // is a deliberate step, with its own prefix.
        backup: Default::default(),
        restore: Some(RestoreSpec {
            source_cluster: source.name.clone(),
            repository: source.backup.clone(),
            backup_label: req
                .backup_label
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            target_time: req
                .target_time
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        }),
    }
}

/// Restore this cluster's backup into a new cluster.
#[utoipa::path(
    post,
    path = "/api/v1/clusters/{name}/restore",
    tag = "backups",
    params(("name" = String, Path, description = "Source cluster name")),
    request_body = RestoreRequest,
    responses(
        (status = 201, description = "Restored cluster created", body = ClusterSpec),
        (status = 404, description = "Source cluster not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "Target exists, or the source has no repository", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 422, description = "Invalid request", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn restore_cluster(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
    Json(req): Json<RestoreRequest>,
) -> Result<(StatusCode, Json<ClusterSpec>), ApiError> {
    let ops = ops(&state)?;
    let Some((source, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };
    if !source.backup.enabled {
        return Err(ApiError::conflict("backups_disabled").with_detail(
            "This cluster has no backup repository, so there is nothing to restore from.",
        ));
    }
    if req.target_cluster.trim() == name {
        return Err(ApiError::conflict("restore_in_place").with_detail(
            "A restore always creates a new cluster. Restoring over the source would \
             destroy the data you are trying to recover; choose a different name.",
        ));
    }

    let spec = restored_spec(&source, &req);
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
    tracing::info!(
        source = %name,
        target = %spec.name,
        label = req.backup_label.as_deref().unwrap_or("latest"),
        "restore requested"
    );
    Ok((StatusCode::CREATED, Json(spec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> ClusterSpec {
        serde_json::from_value(serde_json::json!({
            "name": "src",
            "replicas": 3,
            "image": "pglord-agent:1.2.3",
            "storage_size": "10Gi",
            "parameters": { "work_mem": "32MB" },
            "backup": {
                "enabled": true,
                "s3_endpoint": "https://s3.example.com",
                "s3_bucket": "pg-backups",
                "path_prefix": "team-a",
                "s3_credentials_secret": "backup-creds"
            }
        }))
        .expect("source spec")
    }

    fn request(target: &str) -> RestoreRequest {
        RestoreRequest {
            target_cluster: target.to_string(),
            backup_label: None,
            target_time: None,
            replicas: None,
        }
    }

    #[test]
    fn the_restored_cluster_reads_the_sources_repository() {
        let spec = restored_spec(&source(), &request("copy"));
        let restore = spec.restore.as_ref().expect("restore block");
        assert_eq!(restore.source_cluster, "src");
        // The whole repository is carried over, prefix included: without it the
        // restore looks in the wrong path and finds no stanza.
        assert_eq!(restore.repository.path_prefix.as_deref(), Some("team-a"));
        assert_eq!(restore.repository.s3_bucket.as_deref(), Some("pg-backups"));
        assert_eq!(spec.validate(), Ok(()));
    }

    #[test]
    fn backups_are_off_on_the_restored_cluster() {
        // Inheriting the source's repository would aim a second cluster at the
        // source's stanza; the system identifiers differ and pgBackRest refuses.
        let spec = restored_spec(&source(), &request("copy"));
        assert!(!spec.backup.enabled);
        assert_eq!(spec.backup.s3_bucket, None);
    }

    #[test]
    fn shape_is_inherited_but_replicas_can_be_overridden() {
        let spec = restored_spec(&source(), &request("copy"));
        assert_eq!(spec.replicas, 3);
        assert_eq!(spec.image, "pglord-agent:1.2.3");
        assert_eq!(spec.storage_size, "10Gi");
        assert_eq!(spec.parameters["work_mem"], "32MB");

        let mut req = request("copy");
        req.replicas = Some(1);
        assert_eq!(restored_spec(&source(), &req).replicas, 1);
    }

    #[test]
    fn blank_selectors_mean_latest_rather_than_an_empty_target() {
        // A UI sending "" for an untouched field must not produce an empty
        // --set= or --target=, which pgBackRest would reject.
        let mut req = request("copy");
        req.backup_label = Some("  ".into());
        req.target_time = Some("".into());
        let restore = restored_spec(&source(), &req).restore.expect("restore");
        assert_eq!(restore.backup_label, None);
        assert_eq!(restore.target_time, None);
    }

    #[test]
    fn a_point_in_time_request_is_carried_through_and_validated() {
        let mut req = request("copy");
        req.target_time = Some("2026-08-01T12:30:00Z".into());
        let spec = restored_spec(&source(), &req);
        assert_eq!(spec.validate(), Ok(()));
        assert_eq!(
            spec.restore.expect("restore").target_time.as_deref(),
            Some("2026-08-01T12:30:00Z")
        );

        let mut bad = request("copy");
        bad.target_time = Some("last tuesday".into());
        assert!(restored_spec(&source(), &bad).validate().is_err());
    }

    #[test]
    fn the_target_name_is_trimmed_and_validated() {
        let spec = restored_spec(&source(), &request("  copy  "));
        assert_eq!(spec.name, "copy");
        assert_eq!(spec.validate(), Ok(()));
        // An invalid name has to fail here rather than at apply time.
        assert!(
            restored_spec(&source(), &request("Copy_1"))
                .validate()
                .is_err()
        );
    }
}
