//! What a cluster needs to exist in its namespace, and whether it does.
//!
//! The point is not documentation — it is being able to see, before or after
//! creating a cluster, exactly which Secrets you have to create yourself and
//! which keys they must contain. Contents are never read beyond key names.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::{Api, Patch, PatchParams};
use pglord_core::deps::{RequiredResource, ResourceKind, required_resources};
use pglord_core::render::{
    AGENT_HTTP_PORT, POOLER_PASSWORD_KEY, REPLICATION_PASSWORD_KEY, SUPERUSER_PASSWORD_KEY,
    credentials_secret_name, pod_fqdn,
};
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
    path = "/api/v1/namespaces/{ns}/clusters/{name}/secrets",
    tag = "clusters",
    params(
        ("ns" = String, Path, description = "Namespace"),
        ("name" = String, Path, description = "Cluster name")
    ),
    responses(
        (status = 200, description = "Required Secrets and ConfigMaps", body = ResourceList),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn list_required_secrets(
    State(state): State<Option<ClusterOps>>,
    Path((ns, name)): Path<(String, String)>,
) -> Result<Json<ResourceList>, ApiError> {
    let ops = ops(&state)?;
    let Some((spec, _owner)) = ops
        .store
        .get(&ns, &name)
        .await
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::not_found());
    };

    let secrets: Api<Secret> = Api::namespaced(ops.kube.clone(), &ns);
    let config_maps: Api<ConfigMap> = Api::namespaced(ops.kube.clone(), &ns);

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

/// Rotate the managed credentials: new generated passwords are written to the
/// Secret, applied on the primary, then every instance is restarted.
#[utoipa::path(
    post,
    path = "/api/v1/namespaces/{ns}/clusters/{name}/rotate-credentials",
    tag = "clusters",
    params(
        ("ns" = String, Path, description = "Namespace"),
        ("name" = String, Path, description = "Cluster name")
    ),
    responses(
        (status = 202, description = "Rotation started"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "Cluster uses a user-supplied credentials Secret", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn rotate_credentials(
    State(state): State<Option<ClusterOps>>,
    Path((ns, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let Some((spec, _owner)) = ops
        .store
        .get(&ns, &name)
        .await
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::not_found());
    };
    if spec.credentials_secret().is_some() {
        return Err(ApiError::conflict("custom_credentials_secret").with_detail(
            "This cluster reads credentials from your own Secret; pglord only rotates the Secret it manages.",
        ));
    }
    crate::api::clusters::reject_hibernated(&spec)?;
    let primary = ops
        .store
        .primary(&ns, &name)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| spec.instance_name(0));

    let ops = ops.clone();
    let rot_ns = ns.clone();
    tokio::spawn(async move {
        let outcome = match run_rotation(&ops, &rot_ns, &spec, &primary).await {
            Ok(()) => {
                tracing::info!(cluster = %spec.name, "credentials rotation finished");
                "succeeded"
            }
            Err(err) => {
                tracing::error!(cluster = %spec.name, %err, "credentials rotation failed; re-POST to converge");
                "failed"
            }
        };
        ops.metrics
            .rotations_total
            .with_label_values(&[rot_ns.as_str(), spec.name.as_str(), outcome])
            .inc();
    });
    Ok(StatusCode::ACCEPTED)
}

/// The four idempotent steps. Any failure leaves a state a re-run converges:
/// the Secret write and the ALTERs are both last-writer-wins, and the roll is
/// a plain restart.
async fn run_rotation(
    ops: &ClusterOps,
    ns: &str,
    spec: &pglord_core::spec::ClusterSpec,
    primary: &str,
) -> anyhow::Result<()> {
    let su_pw = crate::reconciler::generate_password();
    let repl_pw = crate::reconciler::generate_password();
    // The pooler's credential rotates with the rest; the reconciler re-renders
    // the pooler config Secret from the new value and the config-hash
    // annotation rolls the pooler pods on its next pass.
    let pooler_pw = spec
        .pooler
        .enabled
        .then(crate::reconciler::generate_password);

    let secrets: Api<Secret> = Api::namespaced(ops.kube.clone(), ns);
    let mut string_data = serde_json::json!({
        SUPERUSER_PASSWORD_KEY: su_pw.clone(),
        REPLICATION_PASSWORD_KEY: repl_pw.clone(),
    });
    if let (Some(pw), Some(map)) = (&pooler_pw, string_data.as_object_mut()) {
        map.insert(
            POOLER_PASSWORD_KEY.to_string(),
            serde_json::Value::String(pw.clone()),
        );
    }
    let patch = serde_json::json!({
        "metadata": { "annotations": {
            "pglord.dev/rotated-at": chrono::Utc::now().to_rfc3339(),
        } },
        "stringData": string_data,
    });
    secrets
        .patch(
            &credentials_secret_name(&spec.name),
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await?;
    tracing::info!(cluster = %spec.name, "rotation: secret updated");

    let url = format!(
        "http://{}:{}/roles/apply",
        pod_fqdn(primary, &spec.name, ns),
        AGENT_HTTP_PORT
    );
    let resp = ops
        .http
        .post(&url)
        .json(&serde_json::json!({
            "superuser_password": su_pw,
            "repl_password": repl_pw,
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("agent /roles/apply on {primary} answered {status}: {body}");
    }
    tracing::info!(cluster = %spec.name, %primary, "rotation: roles altered on primary");

    if let Some(pw) = &pooler_pw {
        let url = format!(
            "http://{}:{}/pooler/ensure",
            pod_fqdn(primary, &spec.name, ns),
            AGENT_HTTP_PORT
        );
        let resp = ops
            .http
            .post(&url)
            .json(&serde_json::json!({ "password": pw }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("agent /pooler/ensure on {primary} answered {status}: {body}");
        }
        tracing::info!(cluster = %spec.name, %primary, "rotation: pooler role altered on primary");
    }

    let order = crate::api::clusters::roll_order(&spec.sts_name(), spec.replicas, primary);
    crate::api::clusters::roll_pods(ops, ns, spec, order).await;
    Ok(())
}
