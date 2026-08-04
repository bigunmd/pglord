//! Operator-mode front loop: watches PostgresCluster resources and converges
//! each one's mirror spec ConfigMap, then reports status on the CR. The
//! mirror carries the same labels standalone specs do, so the reconciler,
//! orchestrator and backup scheduler run identically in both modes.
//!
//! Signal-only watch, the reconciler's pattern: events wake the loop, the
//! work is always a full sync, RESYNC_INTERVAL is the backstop.
//!
//! Status simplification (stage A): `phase` never reports sync-degraded —
//! the cluster-detail API remains the surface for that signal.

use std::time::Duration;

use futures::TryStreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::watcher;
use pglord_core::crd::{PostgresCluster, PostgresClusterStatus};
use pglord_core::spec::ClusterSpec;

use crate::crd_store::{cr_owner_ref, effective_spec};
use crate::primary_record::recorded_primary;
use crate::spec_store::{FIELD_MANAGER, spec_cm_name, spec_to_configmap_owned};

const RESYNC_INTERVAL: Duration = Duration::from_secs(60);
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

pub enum SyncDecision {
    Sync(Box<ClusterSpec>),
    Invalid(String),
}

/// Name rule + validation, pure. metadata.name is the CR's identity; a
/// non-empty spec.name must agree with it, and the effective spec must be
/// valid before anything is mirrored — an invalid kubectl edit never
/// touches a running cluster.
pub fn decide(metadata_name: &str, effective: &ClusterSpec) -> SyncDecision {
    if effective.name != metadata_name {
        return SyncDecision::Invalid(format!(
            "spec.name '{}' does not match metadata.name '{}'",
            effective.name, metadata_name
        ));
    }
    match effective.validate() {
        Ok(()) => SyncDecision::Sync(Box::new(effective.clone())),
        Err(e) => SyncDecision::Invalid(e.to_string()),
    }
}

pub fn phase_for(spec: &ClusterSpec, ready: i32, sync_degraded: bool) -> &'static str {
    if spec.hibernated {
        "Hibernated"
    } else if ready == 0 {
        "Progressing"
    } else if i32::from(spec.replicas) == ready && !sync_degraded {
        "Ready"
    } else {
        "Degraded"
    }
}

pub async fn run(client: kube::Client, ns: String, all_namespaces: bool) {
    let crs: Api<PostgresCluster> = if all_namespaces {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &ns)
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
    let watch_crs = crs.clone();
    tokio::spawn(async move {
        loop {
            let stream = watcher(watch_crs.clone(), watcher::Config::default());
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
            tracing::warn!(?result, "postgrescluster watcher stream ended; restarting");
            tokio::time::sleep(ERROR_BACKOFF).await;
        }
    });

    loop {
        if let Err(err) = sync_all(&client, &crs, &ns).await {
            tracing::warn!(%err, "cr sync pass failed");
            tokio::time::sleep(ERROR_BACKOFF).await;
        }
        tokio::select! {
            _ = rx.recv() => {},
            () = tokio::time::sleep(RESYNC_INTERVAL) => {},
        }
    }
}

async fn sync_all(
    client: &kube::Client,
    crs: &Api<PostgresCluster>,
    ns: &str,
) -> anyhow::Result<()> {
    let list = crs.list(&Default::default()).await?;
    for cr in &list.items {
        // Each CR's mirror and status live in the CR's own namespace.
        let item_ns = cr
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| ns.to_string());
        if let Err(err) = sync_one(client, &item_ns, cr).await {
            let name = cr.metadata.name.as_deref().unwrap_or("?");
            tracing::warn!(cluster = %name, %err, "cr sync failed; next pass retries");
        }
    }
    Ok(())
}

async fn sync_one(client: &kube::Client, ns: &str, cr: &PostgresCluster) -> anyhow::Result<()> {
    let crs: Api<PostgresCluster> = Api::namespaced(client.clone(), ns);
    let name = cr.metadata.name.clone().unwrap_or_default();
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let effective = effective_spec(cr);

    let status = match decide(&name, &effective) {
        SyncDecision::Invalid(msg) => PostgresClusterStatus {
            phase: "Invalid".into(),
            message: msg,
            ..Default::default()
        },
        SyncDecision::Sync(spec) => {
            // Adoption guard: never clobber a cluster this CR does not own —
            // a standalone ConfigMap-backed cluster keeps its name.
            let existing = cms.get_opt(&spec_cm_name(&spec.name)).await?;
            let owned_by_me = existing.as_ref().is_none_or(|cm| {
                cm.metadata.owner_references.as_ref().is_some_and(|refs| {
                    refs.iter()
                        .any(|r| r.uid == cr.metadata.uid.clone().unwrap_or_default())
                })
            });
            if !owned_by_me {
                PostgresClusterStatus {
                    phase: "Invalid".into(),
                    message: format!("name conflict: a cluster '{}' already exists", spec.name),
                    ..Default::default()
                }
            } else {
                let mirror = spec_to_configmap_owned(&spec, ns, &cr_owner_ref(cr))?;
                let mirrored = cms
                    .patch(
                        &spec_cm_name(&spec.name),
                        &PatchParams::apply(FIELD_MANAGER).force(),
                        &Patch::Apply(&mirror),
                    )
                    .await?;
                observe_status(client, ns, &spec, &mirrored).await
            }
        }
    };
    write_status_if_changed(&crs, cr, status).await
}

/// Compute status from the mirror (record annotation) and the StatefulSet.
async fn observe_status(
    client: &kube::Client,
    ns: &str,
    spec: &ClusterSpec,
    mirror: &ConfigMap,
) -> PostgresClusterStatus {
    let stss: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    let ready = match stss.get_opt(&spec.sts_name()).await {
        Ok(Some(sts)) => sts.status.and_then(|s| s.ready_replicas).unwrap_or(0),
        _ => 0,
    };
    PostgresClusterStatus {
        ready_replicas: ready,
        primary: recorded_primary(mirror, &spec.instance_name(0)),
        hibernated: spec.hibernated,
        phase: phase_for(spec, ready, false).to_string(),
        message: String::new(),
    }
}

async fn write_status_if_changed(
    crs: &Api<PostgresCluster>,
    cr: &PostgresCluster,
    status: PostgresClusterStatus,
) -> anyhow::Result<()> {
    if cr.status.as_ref() == Some(&status) {
        return Ok(());
    }
    let name = cr.metadata.name.clone().unwrap_or_default();
    let patch = serde_json::json!({
        "apiVersion": "pglord.dev/v1alpha1",
        "kind": "PostgresCluster",
        "status": status,
    });
    crs.patch_status(
        &name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&patch),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: serde_json::Value) -> ClusterSpec {
        serde_json::from_value(json).expect("spec")
    }

    #[test]
    fn phase_reflects_spec_and_readiness() {
        let s = spec(serde_json::json!({ "name": "demo", "replicas": 2 }));
        assert_eq!(phase_for(&s, 2, false), "Ready");
        assert_eq!(phase_for(&s, 1, false), "Degraded");
        assert_eq!(phase_for(&s, 0, false), "Progressing");
        assert_eq!(phase_for(&s, 2, true), "Degraded");
        let h = spec(serde_json::json!({ "name": "demo", "replicas": 2, "hibernated": true }));
        assert_eq!(phase_for(&h, 0, false), "Hibernated");
    }

    #[test]
    fn name_mismatch_and_invalid_specs_are_flagged_not_synced() {
        let ok = spec(serde_json::json!({ "name": "demo" }));
        assert!(matches!(decide("demo", &ok), SyncDecision::Sync(_)));
        let renamed = spec(serde_json::json!({ "name": "other" }));
        assert!(matches!(decide("demo", &renamed), SyncDecision::Invalid(_)));
        let bad = spec(serde_json::json!({ "name": "demo", "replicas": 99 }));
        assert!(matches!(decide("demo", &bad), SyncDecision::Invalid(_)));
    }
}
