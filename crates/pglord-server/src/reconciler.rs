//! Level-triggered reconciliation: spec ConfigMaps -> StatefulSet + Services.
//! Watch events and a periodic resync both funnel into the same full-sync
//! pass; every apply is server-side (field manager "pglord"), so the loop is
//! idempotent by construction. Deletion needs no code path here: children
//! carry ownerReferences to the spec ConfigMap and are garbage-collected.

use std::time::Duration;

use std::collections::BTreeMap;

use futures::TryStreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::ServiceAccount;
use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::watcher;
use pglord_core::render::{
    OwnerRef, REPLICATION_PASSWORD_KEY, SUPERUSER_PASSWORD_KEY, credentials_secret_name, labels,
    render_headless_service, render_instance_rbac, render_rw_service, render_statefulset,
};
use pglord_core::spec::ClusterSpec;
use rand::RngExt;

use crate::primary_record::recorded_primary;

use crate::spec_store::{FIELD_MANAGER, configmap_to_spec, spec_label_selector};

const RESYNC_INTERVAL: Duration = Duration::from_secs(60);
const ERROR_BACKOFF: Duration = Duration::from_secs(10);

/// Desired children for one cluster. Pure; unit-tested.
pub fn desired_objects(
    spec: &ClusterSpec,
    ns: &str,
    owner: &OwnerRef,
    primary_pod: &str,
) -> (StatefulSet, Service, Service) {
    (
        render_statefulset(spec, ns, owner),
        render_headless_service(spec, ns, owner),
        render_rw_service(spec, ns, owner, primary_pod),
    )
}

pub async fn run(client: kube::Client, ns: String) {
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), &ns);
    let watch_cfg = watcher::Config::default().labels(&spec_label_selector());

    // Watch events only wake the loop; the work is always a full sync so a
    // missed event can never leave state stale for longer than RESYNC_INTERVAL.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
    let watch_cms = cms.clone();
    tokio::spawn(async move {
        loop {
            let stream = watcher(watch_cms.clone(), watch_cfg.clone());
            let tx = tx.clone();
            let result = stream
                .try_for_each(|_event| {
                    let tx = tx.clone();
                    async move {
                        let _ = tx.try_send(());
                        Ok(())
                    }
                })
                .await;
            tracing::warn!(?result, "spec watcher stream ended; restarting");
            tokio::time::sleep(ERROR_BACKOFF).await;
        }
    });

    loop {
        if let Err(err) = sync_all(&client, &cms, &ns).await {
            tracing::warn!(%err, "reconcile pass failed");
            tokio::time::sleep(ERROR_BACKOFF).await;
        }
        tokio::select! {
            _ = rx.recv() => {},
            () = tokio::time::sleep(RESYNC_INTERVAL) => {},
        }
    }
}

async fn sync_all(client: &kube::Client, cms: &Api<ConfigMap>, ns: &str) -> anyhow::Result<()> {
    let specs = cms
        .list(&kube::api::ListParams::default().labels(&spec_label_selector()))
        .await?;
    for cm in &specs.items {
        let (spec, owner) = match configmap_to_spec(cm) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::warn!(cm = ?cm.metadata.name, %err, "skipping unparseable spec");
                continue;
            }
        };
        if let Err(err) = spec.validate() {
            tracing::warn!(cluster = %spec.name, %err, "skipping invalid spec");
            continue;
        }
        let primary_pod = recorded_primary(cm, &spec.name);
        if let Err(err) = apply_cluster(client, ns, &spec, &owner, &primary_pod).await {
            tracing::warn!(cluster = %spec.name, %err, "apply failed");
        } else {
            tracing::debug!(cluster = %spec.name, "reconciled");
        }
    }
    Ok(())
}

/// PVC name a StatefulSet gives instance `ordinal` for the `data` template.
pub fn pvc_name(cluster: &str, ordinal: u8) -> String {
    format!("data-{cluster}-{ordinal}")
}

/// Grow every instance's PVC. StatefulSet volumeClaimTemplates are immutable,
/// so expansion happens on the claims themselves; the StorageClass must allow
/// expansion or the apiserver rejects the patch (surfaced to the caller).
pub async fn expand_pvcs(
    client: &kube::Client,
    ns: &str,
    cluster: &str,
    replicas: u8,
    storage_size: &str,
) -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    let patch = serde_json::json!({
        "spec": { "resources": { "requests": { "storage": storage_size } } }
    });
    for ordinal in 0..replicas {
        let name = pvc_name(cluster, ordinal);
        if api.get_opt(&name).await?.is_none() {
            continue; // not created yet; the template covers it
        }
        api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
    }
    Ok(())
}

/// Delete PVCs left behind by a scale-down (StatefulSets keep them).
/// Best-effort: the spec change has already been applied, so failures here are
/// logged rather than surfaced.
pub async fn delete_orphan_pvcs(
    client: &kube::Client,
    ns: &str,
    cluster: &str,
    from_ordinal: u8,
    to_ordinal: u8,
) {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    for ordinal in from_ordinal..to_ordinal {
        let name = pvc_name(cluster, ordinal);
        match api.delete(&name, &Default::default()).await {
            Ok(_) => tracing::info!(pvc = %name, "deleted PVC of removed instance"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(err) => tracing::warn!(pvc = %name, %err, "failed to delete orphan PVC"),
        }
    }
}

pub fn generate_password() -> String {
    rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// Create-only: an existing credentials Secret is never touched, so passwords
/// survive reconciles (rotation is a future, explicit operation).
async fn ensure_credentials(
    client: &kube::Client,
    ns: &str,
    spec: &ClusterSpec,
    owner: &OwnerRef,
) -> anyhow::Result<()> {
    // The user pointed this cluster at their own Secret: it is theirs to
    // create, own and rotate. Generating anything here would give the cluster
    // two sources of truth for its password, and deleting the cluster would
    // take a Secret we never made.
    if let Some(supplied) = spec.credentials_secret() {
        tracing::debug!(cluster = %spec.name, secret = %supplied, "using supplied credentials secret");
        return Ok(());
    }
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let name = credentials_secret_name(&spec.name);
    if secrets.get_opt(&name).await?.is_some() {
        return Ok(());
    }
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(ns.to_string()),
            labels: Some(labels(&spec.name)),
            owner_references: Some(vec![OwnerReference {
                api_version: owner.api_version.clone(),
                kind: owner.kind.clone(),
                name: owner.name.clone(),
                uid: owner.uid.clone(),
                controller: Some(true),
                block_owner_deletion: Some(true),
            }]),
            ..Default::default()
        },
        string_data: Some(BTreeMap::from([
            (SUPERUSER_PASSWORD_KEY.to_string(), generate_password()),
            (REPLICATION_PASSWORD_KEY.to_string(), generate_password()),
        ])),
        ..Default::default()
    };
    match secrets.create(&PostParams::default(), &secret).await {
        Ok(_) => {
            tracing::info!(cluster = %spec.name, "credentials secret created");
            Ok(())
        }
        // Lost a race with a concurrent create: fine, it exists.
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub async fn apply_cluster(
    client: &kube::Client,
    ns: &str,
    spec: &ClusterSpec,
    owner: &OwnerRef,
    primary_pod: &str,
) -> anyhow::Result<()> {
    ensure_credentials(client, ns, spec, owner).await?;
    let (sa, role, binding) = render_instance_rbac(spec, ns, owner);
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), ns);
    let sa_name = sa.metadata.name.clone().unwrap_or_default();
    sa_api.patch(&sa_name, &pp, &Patch::Apply(&sa)).await?;
    let role_api: Api<Role> = Api::namespaced(client.clone(), ns);
    role_api.patch(&sa_name, &pp, &Patch::Apply(&role)).await?;
    let rb_api: Api<RoleBinding> = Api::namespaced(client.clone(), ns);
    rb_api.patch(&sa_name, &pp, &Patch::Apply(&binding)).await?;

    let (sts, headless, rw) = desired_objects(spec, ns, owner, primary_pod);

    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    sts_api.patch(&spec.name, &pp, &Patch::Apply(&sts)).await?;

    let svc_api: Api<Service> = Api::namespaced(client.clone(), ns);
    for svc in [headless, rw] {
        let name = svc.metadata.name.clone().unwrap_or_default();
        svc_api.patch(&name, &pp, &Patch::Apply(&svc)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pvc_names_match_statefulset_convention() {
        // <volumeClaimTemplate>-<statefulset>-<ordinal>
        assert_eq!(pvc_name("demo", 0), "data-demo-0");
        assert_eq!(pvc_name("orders-db", 2), "data-orders-db-2");
    }

    #[test]
    fn generated_passwords_are_long_alphanumeric_and_unique() {
        let a = generate_password();
        let b = generate_password();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }

    #[test]
    fn desired_objects_are_consistently_named() {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "demo", "replicas": 2 }))
                .expect("spec");
        let owner = OwnerRef {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            name: "demo-spec".into(),
            uid: "u".into(),
        };
        let (sts, headless, rw) = desired_objects(&spec, "ns", &owner, "demo-0");
        assert_eq!(sts.metadata.name.as_deref(), Some("demo"));
        assert_eq!(headless.metadata.name.as_deref(), Some("demo-pods"));
        assert_eq!(rw.metadata.name.as_deref(), Some("demo-rw"));
        assert_eq!(
            sts.spec.expect("spec").service_name.as_deref(),
            Some("demo-pods")
        );
    }
}
