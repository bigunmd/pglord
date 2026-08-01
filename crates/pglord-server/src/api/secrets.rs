//! What a cluster needs to exist in its namespace, and whether it does.
//!
//! The point is not documentation — it is being able to see, before or after
//! creating a cluster, exactly which Secrets you have to create yourself and
//! which keys they must contain. Contents are never read beyond key names.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::Api;
use pglord_core::deps::{RequiredResource, ResourceKind, required_resources};
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::ClusterOps;
use crate::error::ApiError;

#[derive(Serialize, ToSchema)]
pub struct ResourceStatus {
    #[serde(flatten)]
    pub required: RequiredResource,
    /// The object exists in the namespace.
    pub exists: bool,
    /// Required keys the object does not have. Empty when it is absent
    /// entirely — that is what `exists` is for.
    pub missing_keys: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ResourceList {
    /// Nothing the cluster depends on is missing or incomplete.
    pub satisfied: bool,
    pub resources: Vec<ResourceStatus>,
}

fn ops(state: &Option<ClusterOps>) -> Result<&ClusterOps, ApiError> {
    state.as_ref().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "kube_unavailable")
            .with_detail("No Kubernetes environment available.")
    })
}

/// A resource pglord creates itself is reported as satisfied even before the
/// reconciler has got to it: telling someone to go and create `demo-credentials`
/// by hand would be wrong.
fn is_blocking(status: &ResourceStatus) -> bool {
    !status.required.managed && (!status.exists || !status.missing_keys.is_empty())
}

/// List the Secrets and ConfigMaps this cluster depends on.
#[utoipa::path(
    get,
    path = "/api/v1/clusters/{name}/secrets",
    tag = "clusters",
    params(("name" = String, Path, description = "Cluster name")),
    responses(
        (status = 200, description = "Required Secrets and ConfigMaps", body = ResourceList),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn list_required_secrets(
    State(state): State<Option<ClusterOps>>,
    Path(name): Path<String>,
) -> Result<Json<ResourceList>, ApiError> {
    let ops = ops(&state)?;
    let Some((spec, _owner)) = ops.store.get(&name).await.map_err(ApiError::internal)? else {
        return Err(ApiError::not_found());
    };

    let secrets: Api<Secret> = Api::namespaced(ops.kube.clone(), &ops.ns);
    let config_maps: Api<ConfigMap> = Api::namespaced(ops.kube.clone(), &ops.ns);

    let mut resources = Vec::new();
    for required in required_resources(&spec) {
        // Only key *names* are read. A Secret's values are never fetched into
        // the server, let alone returned over the API.
        let present_keys: Option<Vec<String>> = match required.kind {
            ResourceKind::Secret => secrets
                .get_opt(&required.name)
                .await
                .map_err(ApiError::internal)?
                .map(|s| s.data.unwrap_or_default().into_keys().collect()),
            ResourceKind::ConfigMap => config_maps
                .get_opt(&required.name)
                .await
                .map_err(ApiError::internal)?
                .map(|c| c.data.unwrap_or_default().into_keys().collect()),
        };
        let (exists, missing_keys) = match present_keys {
            Some(present) => {
                let missing = required
                    .keys
                    .iter()
                    .filter(|k| !present.contains(k))
                    .cloned()
                    .collect();
                (true, missing)
            }
            None => (false, Vec::new()),
        };
        resources.push(ResourceStatus {
            required,
            exists,
            missing_keys,
        });
    }

    Ok(Json(ResourceList {
        satisfied: !resources.iter().any(is_blocking),
        resources,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pglord_core::deps::ResourcePurpose;

    fn status(managed: bool, exists: bool, missing: &[&str]) -> ResourceStatus {
        ResourceStatus {
            required: RequiredResource {
                name: "s".into(),
                kind: ResourceKind::Secret,
                purpose: ResourcePurpose::Credentials,
                keys: vec!["k".into()],
                managed,
            },
            exists,
            missing_keys: missing.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn only_user_owned_resources_can_block() {
        // pglord creates its own credentials Secret on the next reconcile;
        // reporting it as missing would send someone off to create it by hand.
        assert!(!is_blocking(&status(true, false, &[])));
        assert!(is_blocking(&status(false, false, &[])));
    }

    #[test]
    fn an_existing_secret_missing_a_key_still_blocks() {
        // This is the failure that otherwise shows up as a pod stuck in
        // CreateContainerConfigError with the reason buried in pod events.
        assert!(is_blocking(&status(false, true, &["secret-key"])));
        assert!(!is_blocking(&status(false, true, &[])));
    }
}
