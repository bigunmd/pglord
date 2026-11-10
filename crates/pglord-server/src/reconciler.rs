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
    AGENT_HTTP_PORT, OwnerRef, POOLER_PASSWORD_KEY, REPLICATION_PASSWORD_KEY,
    SUPERUSER_PASSWORD_KEY, credentials_secret_for, credentials_secret_name, labels, pod_fqdn,
    pool_service_name, pooler_config_secret_name, pooler_deployment_name, render_headless_service,
    render_instance_rbac, render_pool_service, render_pooler_config_secret,
    render_pooler_deployment, render_rw_service,
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
    upgrade: Option<&pglord_core::upgrade::UpgradeState>,
) -> (StatefulSet, Service, Service) {
    (
        pglord_core::render::render_statefulset_for_upgrade(spec, ns, owner, upgrade),
        render_headless_service(spec, ns, owner),
        render_rw_service(spec, ns, owner, primary_pod),
    )
}

pub async fn run(
    client: kube::Client,
    ns: String,
    all_namespaces: bool,
    metrics: crate::metrics::Metrics,
) {
    // For the best-effort pooler auth ensure against the primary's agent.
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(http) => http,
        Err(err) => {
            tracing::error!(%err, "reconciler http client failed to build; disabled");
            return;
        }
    };
    let cms: Api<ConfigMap> = if all_namespaces {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &ns)
    };
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

    let mut known: std::collections::HashSet<(String, String)> = Default::default();
    loop {
        if let Err(err) = sync_all(&client, &http, &cms, &ns, &metrics, &mut known).await {
            tracing::warn!(%err, "reconcile pass failed");
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
    http: &reqwest::Client,
    cms: &Api<ConfigMap>,
    ns: &str,
    metrics: &crate::metrics::Metrics,
    known: &mut std::collections::HashSet<(String, String)>,
) -> anyhow::Result<()> {
    let specs = cms
        .list(&kube::api::ListParams::default().labels(&spec_label_selector()))
        .await?;
    metrics.clusters.set(specs.items.len() as i64);
    let mut current = std::collections::HashSet::new();
    for cm in &specs.items {
        let (spec, owner) = match configmap_to_spec(cm) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::warn!(cm = ?cm.metadata.name, %err, "skipping unparseable spec");
                continue;
            }
        };
        // Cluster-wide watches carry items from many namespaces; every apply
        // targets the item's own.
        let item_ns = cm
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| ns.to_string());
        current.insert((item_ns.clone(), spec.name.clone()));
        if let Err(err) = spec.validate() {
            tracing::warn!(cluster = %spec.name, %err, "skipping invalid spec");
            continue;
        }
        let primary_pod = recorded_primary(cm, &spec.instance_name(0));
        let upgrade = crate::spec_store::upgrade_from_cm(cm);
        if let Err(err) = apply_cluster(
            client,
            http,
            &item_ns,
            &spec,
            &owner,
            &primary_pod,
            upgrade.as_ref(),
        )
        .await
        {
            metrics
                .reconcile_total
                .with_label_values(&[item_ns.as_str(), spec.name.as_str(), "error"])
                .inc();
            tracing::warn!(cluster = %spec.name, ns = %item_ns, %err, "apply failed");
        } else {
            metrics
                .reconcile_total
                .with_label_values(&[item_ns.as_str(), spec.name.as_str(), "ok"])
                .inc();
            tracing::debug!(cluster = %spec.name, ns = %item_ns, "reconciled");
        }
    }
    for (removed_ns, removed) in known.difference(&current) {
        metrics.forget_cluster(removed_ns, removed);
    }
    *known = current;
    Ok(())
}

/// PVC name a StatefulSet gives instance `ordinal` for the `data` template.
/// `sts` is the StatefulSet's name, which carries the generation suffix after
/// a major upgrade (`ClusterSpec::sts_name`).
pub fn pvc_name(sts: &str, ordinal: u8) -> String {
    format!("data-{sts}-{ordinal}")
}

/// Grow every instance's PVC. StatefulSet volumeClaimTemplates are immutable,
/// so expansion happens on the claims themselves; the StorageClass must allow
/// expansion or the apiserver rejects the patch (surfaced to the caller).
pub async fn expand_pvcs(
    client: &kube::Client,
    ns: &str,
    sts: &str,
    replicas: u8,
    storage_size: &str,
) -> anyhow::Result<()> {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    let patch = serde_json::json!({
        "spec": { "resources": { "requests": { "storage": storage_size } } }
    });
    for ordinal in 0..replicas {
        let name = pvc_name(sts, ordinal);
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
    sts: &str,
    from_ordinal: u8,
    to_ordinal: u8,
) {
    use k8s_openapi::api::core::v1::PersistentVolumeClaim;
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
    for ordinal in from_ordinal..to_ordinal {
        let name = pvc_name(sts, ordinal);
        match api.delete(&name, &Default::default()).await {
            Ok(_) => tracing::info!(pvc = %name, "deleted PVC of removed instance"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(err) => tracing::warn!(pvc = %name, %err, "failed to delete orphan PVC"),
        }
    }
}

/// Storage requested by a live StatefulSet's volume claim template.
pub fn sts_storage_request(sts: &StatefulSet) -> Option<String> {
    sts.spec
        .as_ref()?
        .volume_claim_templates
        .as_ref()?
        .first()?
        .spec
        .as_ref()?
        .resources
        .as_ref()?
        .requests
        .as_ref()?
        .get("storage")
        .map(|q| q.0.clone())
}

/// Whether the StatefulSet has to be recreated rather than patched.
///
/// `volumeClaimTemplates` are immutable: the apiserver rejects the whole update
/// when they change, so after storage grows *every* later reconcile fails with
/// it — scaling included, silently, since the reconciler only logs a warning.
/// Unparseable quantities answer "no": recreating on a value we do not
/// understand risks more than leaving it alone.
pub fn needs_recreate(existing: Option<&str>, desired: &str) -> bool {
    match (
        existing.and_then(pglord_core::spec::quantity_bytes),
        pglord_core::spec::quantity_bytes(desired),
    ) {
        (Some(live), Some(want)) => live != want,
        _ => false,
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
    if let Some(existing) = secrets.get_opt(&name).await? {
        // Create-only for the passwords that already exist — but a pooler
        // enabled after the Secret was generated needs its credential to
        // appear without a manual step. Existing keys are never touched.
        if spec.pooler.enabled
            && !existing
                .data
                .as_ref()
                .is_some_and(|d| d.contains_key(POOLER_PASSWORD_KEY))
        {
            let patch = serde_json::json!({
                "stringData": { POOLER_PASSWORD_KEY: generate_password() }
            });
            secrets
                .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
            tracing::info!(cluster = %spec.name, "pooler password added to credentials secret");
        }
        return Ok(());
    }
    let mut string_data = BTreeMap::from([
        (SUPERUSER_PASSWORD_KEY.to_string(), generate_password()),
        (REPLICATION_PASSWORD_KEY.to_string(), generate_password()),
    ]);
    if spec.pooler.enabled {
        string_data.insert(POOLER_PASSWORD_KEY.to_string(), generate_password());
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
        string_data: Some(string_data),
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

/// Delete the pooler's children when the pooler is disabled. The reconciler
/// otherwise only ever applies; these three are the exception because a
/// disabled feature block must not leave a live Deployment behind. NotFound
/// is success — the normal state for the many clusters with no pooler.
async fn delete_pooler_objects(client: &kube::Client, ns: &str, cluster: &str) {
    use k8s_openapi::api::apps::v1::Deployment;
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let services: Api<Service> = Api::namespaced(client.clone(), ns);
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    for (what, result) in [
        (
            "deployment",
            deployments
                .delete(&pooler_deployment_name(cluster), &Default::default())
                .await
                .map(|_| ()),
        ),
        (
            "service",
            services
                .delete(&pool_service_name(cluster), &Default::default())
                .await
                .map(|_| ()),
        ),
        (
            "secret",
            secrets
                .delete(&pooler_config_secret_name(cluster), &Default::default())
                .await
                .map(|_| ()),
        ),
    ] {
        match result {
            Ok(()) => tracing::info!(cluster, what, "pooler object deleted"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(err) => tracing::warn!(cluster, what, %err, "pooler object delete failed"),
        }
    }
}

/// Apply (or remove) the pooler's children, then best-effort ensure the auth
/// role on the primary. A missing pooler password is logged and skipped
/// rather than failing the pass: the postgres objects are already applied,
/// and required-secrets is the surface that tells the user what to create.
async fn apply_pooler(
    client: &kube::Client,
    http: &reqwest::Client,
    ns: &str,
    spec: &ClusterSpec,
    owner: &OwnerRef,
    primary_pod: &str,
) -> anyhow::Result<()> {
    use k8s_openapi::api::apps::v1::Deployment;
    if !spec.pooler.enabled {
        delete_pooler_objects(client, ns, &spec.name).await;
        return Ok(());
    }
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let secret_name = credentials_secret_for(spec);
    let password = secrets
        .get_opt(&secret_name)
        .await?
        .and_then(|s| s.data)
        .and_then(|d| d.get(POOLER_PASSWORD_KEY).cloned())
        .and_then(|v| String::from_utf8(v.0).ok());
    let Some(password) = password else {
        tracing::warn!(
            cluster = %spec.name,
            secret = %secret_name,
            "no usable {POOLER_PASSWORD_KEY} key; pooler not applied this pass"
        );
        return Ok(());
    };

    let pp = PatchParams::apply(FIELD_MANAGER).force();
    let config = render_pooler_config_secret(spec, ns, owner, &password);
    secrets
        .patch(
            &pooler_config_secret_name(&spec.name),
            &pp,
            &Patch::Apply(&config),
        )
        .await?;
    let hash = pglord_core::pooler::config_hash(
        &pglord_core::pooler::pgbouncer_ini(spec),
        &pglord_core::pooler::userlist(&password),
    );
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let deployment = render_pooler_deployment(spec, ns, owner, &hash);
    deployments
        .patch(
            &pooler_deployment_name(&spec.name),
            &pp,
            &Patch::Apply(&deployment),
        )
        .await?;
    let services: Api<Service> = Api::namespaced(client.clone(), ns);
    let service = render_pool_service(spec, ns, owner);
    services
        .patch(&pool_service_name(&spec.name), &pp, &Patch::Apply(&service))
        .await?;

    // Hibernated: the objects above are applied (the Deployment renders zero
    // replicas) but there is no primary to ensure the auth role on, and a
    // warning every pass would just be noise.
    if spec.hibernated {
        return Ok(());
    }

    // Level-triggered: refused (standby, booting) or unreachable just means
    // the next pass retries — after create, enable, failover and restore
    // alike. Clients fail auth through the pool until this lands once.
    let url = format!(
        "http://{}:{}/pooler/ensure",
        pod_fqdn(primary_pod, &spec.name, ns),
        AGENT_HTTP_PORT
    );
    match http
        .post(&url)
        .json(&serde_json::json!({ "password": password }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => tracing::warn!(
            cluster = %spec.name, status = %resp.status(),
            "pooler auth ensure refused; retrying next pass"
        ),
        Err(err) => tracing::warn!(
            cluster = %spec.name, %err,
            "pooler auth ensure unreachable; retrying next pass"
        ),
    }
    Ok(())
}

/// Converge declared roles and databases via the primary's agent. Same
/// stance as the pooler ensure: level-triggered, best-effort per pass. A
/// missing password Secret logs and skips — required-secrets is the
/// user-facing surface for it — and postgres objects are untouched.
async fn apply_provisioning(
    client: &kube::Client,
    http: &reqwest::Client,
    ns: &str,
    spec: &ClusterSpec,
    primary_pod: &str,
) -> anyhow::Result<()> {
    if spec.roles.is_empty() && spec.databases.is_empty() && spec.extensions.is_empty() {
        return Ok(());
    }
    // No primary to converge on while hibernated; the next pass after wake
    // picks it up.
    if spec.hibernated {
        return Ok(());
    }
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
    let mut roles = Vec::with_capacity(spec.roles.len());
    for role in &spec.roles {
        let password = match &role.password_secret {
            None => None,
            Some(sref) => {
                let value = secrets
                    .get_opt(&sref.name)
                    .await?
                    .and_then(|s| s.data)
                    .and_then(|d| d.get(&sref.key).cloned())
                    .and_then(|v| String::from_utf8(v.0).ok());
                match value {
                    Some(v) => Some(v),
                    None => {
                        tracing::warn!(
                            cluster = %spec.name, role = %role.name,
                            secret = %sref.name, key = %sref.key,
                            "role password secret unusable; provisioning skipped this pass"
                        );
                        return Ok(());
                    }
                }
            }
        };
        roles.push(serde_json::json!({
            "name": role.name,
            "password": password,
            "login": role.login,
            "createdb": role.createdb,
            "createrole": role.createrole,
            "connection_limit": role.connection_limit,
            "in_roles": role.in_roles,
        }));
    }
    let databases: Vec<_> = spec
        .databases
        .iter()
        .map(|d| serde_json::json!({ "name": d.name, "owner": d.owner }))
        .collect();
    let extensions: Vec<_> = spec
        .extensions
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name, "database": e.database,
                "schema": e.schema, "preload": e.preload,
            })
        })
        .collect();
    let url = format!(
        "http://{}:{}/provision/apply",
        pod_fqdn(primary_pod, &spec.name, ns),
        AGENT_HTTP_PORT
    );
    match http
        .post(&url)
        .json(&serde_json::json!({ "roles": roles, "databases": databases, "extensions": extensions }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => tracing::warn!(
            cluster = %spec.name, status = %resp.status(),
            "provisioning ensure refused; retrying next pass"
        ),
        Err(err) => tracing::warn!(
            cluster = %spec.name, %err,
            "provisioning ensure unreachable; retrying next pass"
        ),
    }
    Ok(())
}

/// A fresh observation is a fail-closed stale-work check, not a transaction
/// with child writes: source and child resourceVersions are independent.
struct ApplySource<'a> {
    client: &'a kube::Client,
    ns: &'a str,
    spec: &'a ClusterSpec,
    owner: &'a OwnerRef,
    upgrade: Option<&'a pglord_core::upgrade::UpgradeState>,
}

impl ApplySource<'_> {
    async fn validate(&self) -> anyhow::Result<()> {
        use pglord_core::spec::PostgresCluster;
        let cms: Api<ConfigMap> = Api::namespaced(self.client.clone(), self.ns);
        let cm = cms
            .get(&crate::spec_store::spec_cm_name(&self.spec.name))
            .await?;
        let (current, mirror_owner) = configmap_to_spec(&cm)?;
        anyhow::ensure!(
            cm.metadata.deletion_timestamp.is_none(),
            "spec mirror is deleting"
        );
        anyhow::ensure!(current == *self.spec, "refusing stale desired spec");
        // Unlike upgrade_from_cm, corrupt state must not look like no upgrade.
        let upgrade: Option<pglord_core::upgrade::UpgradeState> = cm
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(pglord_core::upgrade::UPGRADE_ANNOTATION))
            .map(|raw| serde_json::from_str(raw))
            .transpose()?;
        anyhow::ensure!(
            upgrade.as_ref() == self.upgrade,
            "refusing stale desired upgrade state"
        );
        let cr_owner = cm.metadata.owner_references.as_ref().and_then(|owners| {
            owners
                .iter()
                .find(|o| o.controller == Some(true) && o.kind == "PostgresCluster")
        });
        if self.owner.kind == "ConfigMap" {
            anyhow::ensure!(
                self.owner.uid == mirror_owner.uid && self.owner.name == mirror_owner.name,
                "spec mirror identity changed"
            );
        } else {
            let owner = cr_owner.ok_or_else(|| anyhow::anyhow!("CR spec mirror has no owner"))?;
            anyhow::ensure!(
                self.owner.kind == owner.kind
                    && self.owner.api_version == owner.api_version
                    && self.owner.name == owner.name
                    && self.owner.uid == owner.uid,
                "CR spec mirror identity changed"
            );
        }
        if let Some(owner) = cr_owner {
            // A matching mirror can itself lag the authoritative CR.
            let crs: Api<PostgresCluster> = Api::namespaced(self.client.clone(), self.ns);
            let cr = crs.get(&owner.name).await?;
            anyhow::ensure!(
                cr.metadata.deletion_timestamp.is_none()
                    && cr.metadata.uid.as_deref() == Some(owner.uid.as_str())
                    && crate::crd_store::effective_spec(&cr) == *self.spec,
                "spec mirror does not match current CR authority"
            );
        }
        Ok(())
    }
}

/// Apply mutable fields with an object-version precondition; immutable changes
/// orphan the old controller, never its pods or volumes. A competing creator is
/// re-read, not overwritten, and a completed Parallel migration is one-way.
async fn apply_statefulset<F, Fut>(
    api: &Api<StatefulSet>,
    desired: &StatefulSet,
    validate_source: F,
) -> anyhow::Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    use anyhow::Context;
    use kube::api::{DeleteParams, Preconditions, PropagationPolicy};
    let name = desired
        .metadata
        .name
        .as_deref()
        .context("StatefulSet has no name")?;
    for _ in 0..50 {
        let live = api.get_opt(name).await?;
        validate_source().await?;
        let Some(live) = live else {
            match api.create(&PostParams::default(), desired).await {
                Ok(_) => return Ok(()),
                Err(kube::Error::Api(e)) if e.code == 409 => continue,
                Err(err) => return Err(err.into()),
            }
        };
        if live.metadata.deletion_timestamp.is_some() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        if live.metadata.owner_references != desired.metadata.owner_references {
            anyhow::bail!("refusing to replace StatefulSet owned by a different cluster");
        }
        let uid = live
            .metadata
            .uid
            .clone()
            .context("StatefulSet has no UID")?;
        let rv = live
            .metadata
            .resource_version
            .clone()
            .context("StatefulSet has no version")?;
        let policy = |s: &StatefulSet| {
            s.spec
                .as_ref()
                .and_then(|s| s.pod_management_policy.clone())
                .unwrap_or_else(|| "OrderedReady".into())
        };
        if policy(&live) == "Parallel" && policy(desired) != "Parallel" {
            anyhow::bail!("refusing stale desired state that reverses Parallel migration");
        }
        let storage_changed = sts_storage_request(desired)
            .is_some_and(|size| needs_recreate(sts_storage_request(&live).as_deref(), &size));
        if storage_changed || policy(&live) != policy(desired) {
            // Retain is durable before deletion; interruption can only leave
            // extra volumes, never request their garbage collection.
            let retain = serde_json::json!({
                "metadata": {"uid": uid, "resourceVersion": rv},
                "spec": {"persistentVolumeClaimRetentionPolicy": {
                    "whenDeleted": "Retain", "whenScaled": "Retain"
                }}
            });
            let retained = match api
                .patch(name, &PatchParams::default(), &Patch::Merge(&retain))
                .await
            {
                Ok(sts) => sts,
                Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => continue,
                Err(err) => return Err(err.into()),
            };
            let params = DeleteParams {
                propagation_policy: Some(PropagationPolicy::Orphan),
                preconditions: Some(Preconditions {
                    uid: Some(uid),
                    resource_version: Some(
                        retained
                            .metadata
                            .resource_version
                            .context("retention update has no version")?,
                    ),
                }),
                ..Default::default()
            };
            validate_source().await?;
            match api.delete(name, &params).await {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
                Err(err) => return Err(err.into()),
            }
            continue;
        }
        let mut update = desired.clone();
        update.metadata.uid = Some(uid);
        update.metadata.resource_version = Some(rv);
        match api
            .patch(
                name,
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(&update),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => continue,
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("StatefulSet migration did not converge; retry without replacing pods")
}

pub async fn apply_cluster(
    client: &kube::Client,
    http: &reqwest::Client,
    ns: &str,
    spec: &ClusterSpec,
    owner: &OwnerRef,
    primary_pod: &str,
    upgrade: Option<&pglord_core::upgrade::UpgradeState>,
) -> anyhow::Result<()> {
    let source = ApplySource {
        client,
        ns,
        spec,
        owner,
        upgrade,
    };
    source.validate().await?;
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

    let (sts, headless, rw) = desired_objects(spec, ns, owner, primary_pod, upgrade);

    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), ns);
    apply_statefulset(&sts_api, &sts, || source.validate()).await?;

    // A major upgrade in flight renders a second instance set from the same
    // spec, on the target image and one generation up. It hangs off the same
    // owning ConfigMap, so deleting the cluster still collects everything.
    //
    // Once the spec has adopted the target generation (finalize), that set IS
    // the cluster and was already applied above. Rendering it as "green" again
    // would write the subscriber-mode env var back onto the promoted
    // instances — the guard closes the window between finalize's spec write
    // and its clearing of the upgrade state.
    if let Some(upgrade) = upgrade.filter(|u| {
        u.mode == pglord_core::upgrade::UpgradeMode::BlueGreen
            && u.next_generation != spec.generation
    }) {
        let green = pglord_core::render::render_green_statefulset(spec, ns, owner, upgrade);
        apply_statefulset(&sts_api, &green, || source.validate()).await?;
    }

    let svc_api: Api<Service> = Api::namespaced(client.clone(), ns);
    for svc in [headless, rw] {
        let name = svc.metadata.name.clone().unwrap_or_default();
        source.validate().await?;
        svc_api.patch(&name, &pp, &Patch::Apply(&svc)).await?;
    }
    source.validate().await?;
    apply_pooler(client, http, ns, spec, owner, primary_pod).await?;
    source.validate().await?;
    apply_provisioning(client, http, ns, spec, primary_pod).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_claim_template_forces_a_recreate() {
        // volumeClaimTemplates are immutable. Patching a StatefulSet whose
        // template changed is rejected outright, and because the reconciler
        // applies everything in one call, that rejection also swallows the
        // replicas change -- scaling stops working the moment storage grows.
        assert!(needs_recreate(Some("1Gi"), "2Gi"));
        assert!(needs_recreate(Some("2Gi"), "1Gi"));

        // The same size written differently is not a change; recreating on it
        // would churn the StatefulSet on every reconcile.
        assert!(!needs_recreate(Some("1Gi"), "1024Mi"));
        assert!(!needs_recreate(Some("1Gi"), "1Gi"));

        // Nothing to compare against, or a quantity we cannot parse: leave it
        // alone rather than recreate on a guess.
        assert!(!needs_recreate(None, "1Gi"));
        assert!(!needs_recreate(Some("banana"), "1Gi"));
        assert!(!needs_recreate(Some("1Gi"), "banana"));
    }

    #[test]
    fn storage_request_is_read_back_from_the_claim_template() {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "demo", "storage_size": "5Gi" }))
                .expect("spec");
        let owner = OwnerRef {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            name: "demo-spec".into(),
            uid: "u".into(),
        };
        let sts = pglord_core::render::render_statefulset(&spec, "ns", &owner);
        assert_eq!(sts_storage_request(&sts).as_deref(), Some("5Gi"));
        // A StatefulSet with no template at all must not panic or claim 0.
        assert_eq!(sts_storage_request(&StatefulSet::default()), None);
    }

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
        let (sts, headless, rw) = desired_objects(&spec, "ns", &owner, "demo-0", None);
        assert_eq!(sts.metadata.name.as_deref(), Some("demo"));
        assert_eq!(headless.metadata.name.as_deref(), Some("demo-pods"));
        assert_eq!(rw.metadata.name.as_deref(), Some("demo-rw"));
        assert_eq!(
            sts.spec.expect("spec").service_name.as_deref(),
            Some("demo-pods")
        );
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request, Response},
    };
    use http_body_util::BodyExt;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn source_validation_rejects_stale_spec_upgrade_and_cr_mirror() {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({"name":"demo"})).expect("spec");
        for case in [
            "current",
            "spec",
            "upgrade",
            "corrupt",
            "cr",
            "cr_current",
            "recreated",
        ] {
            let mut cm = crate::spec_store::spec_to_configmap(&spec, "ns").expect("mirror");
            cm.metadata.uid = Some("mirror".into());
            if case == "spec" {
                let mut changed = spec.clone();
                changed.replicas += 1;
                cm = crate::spec_store::spec_to_configmap(&changed, "ns").expect("mirror");
                cm.metadata.uid = Some("mirror".into());
            }
            if case == "upgrade" || case == "corrupt" {
                cm.metadata.annotations = Some(
                    [(
                        pglord_core::upgrade::UPGRADE_ANNOTATION.into(),
                        if case == "corrupt" {
                            "{".into()
                        } else {
                            serde_json::json!({"phase":"upgrading", "mode":"in_place",
                                "target_image":"postgres:18", "next_generation":"g1", "from_major":17}).to_string()
                        },
                    )]
                    .into(),
                );
            }
            if case == "recreated" {
                cm.metadata.uid = Some("other".into());
            }
            if case == "cr" || case == "cr_current" {
                cm.metadata.owner_references = Some(vec![OwnerReference {
                    api_version: "pglord.dev/v1alpha1".into(),
                    kind: "PostgresCluster".into(),
                    name: "demo".into(),
                    uid: "cr".into(),
                    controller: Some(true),
                    ..Default::default()
                }]);
            }
            let mut cr = pglord_core::spec::PostgresCluster::new("demo", spec.clone());
            cr.metadata.uid = Some("cr".into());
            if case != "cr_current" {
                cr.spec.replicas += 1;
            }
            let service = tower::service_fn(move |req: Request<kube::client::Body>| {
                assert_eq!(req.method(), Method::GET, "validation must never write");
                let value = if req.uri().path().contains("configmaps") {
                    serde_json::to_value(&cm).expect("cm")
                } else {
                    serde_json::to_value(&cr).expect("cr")
                };
                async move {
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Body::from(value.to_string()))
                            .expect("response"),
                    )
                }
            });
            let client = kube::Client::new(service, "ns");
            let owner = OwnerRef {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                name: "demo-spec".into(),
                uid: "mirror".into(),
            };
            let source = ApplySource {
                client: &client,
                ns: "ns",
                spec: &spec,
                owner: &owner,
                upgrade: None,
            };
            assert_eq!(
                source.validate().await.is_ok(),
                matches!(case, "current" | "cr_current"),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn stale_same_parallel_source_is_rechecked_after_conflict_before_mutation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let desired: StatefulSet = serde_json::from_value(serde_json::json!({
            "metadata":{"name":"demo","namespace":"ns"},
            "spec":{"selector":{"matchLabels":{"app":"demo"}},"serviceName":"demo",
                "template":{"metadata":{"labels":{"app":"demo"}}},"podManagementPolicy":"Parallel"}
        }))
        .expect("desired");
        let mut live = desired.clone();
        live.metadata.uid = Some("uid".into());
        live.metadata.resource_version = Some("fresh-sts-version".into());
        let writes = Arc::new(AtomicUsize::new(0));
        let count = writes.clone();
        let service = tower::service_fn(move |req: Request<kube::client::Body>| {
            let (code, value) = if req.method() == Method::GET {
                (200, serde_json::to_value(&live).expect("live"))
            } else {
                assert_eq!(req.method(), Method::PATCH);
                count.fetch_add(1, Ordering::SeqCst);
                (
                    409,
                    serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure",
                    "reason":"Conflict","message":"new target won","code":409}),
                )
            };
            async move {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(Body::from(value.to_string()))
                        .expect("response"),
                )
            }
        });
        let api = Api::namespaced(kube::Client::new(service, "ns"), "ns");
        let checks = AtomicUsize::new(0);
        let result = apply_statefulset(&api, &desired, || async {
            anyhow::ensure!(
                checks.fetch_add(1, Ordering::SeqCst) == 0,
                "stale upgrade snapshot"
            );
            Ok(())
        })
        .await;
        assert!(
            result
                .expect_err("must stop")
                .to_string()
                .contains("stale upgrade snapshot")
        );
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "never retry with old desired and new STS version"
        );
    }

    async fn migration(interrupt: bool, concurrent: bool) {
        let desired: StatefulSet = serde_json::from_value(serde_json::json!({
            "metadata":{"name":"demo","namespace":"ns"},
            "spec":{"selector":{"matchLabels":{"app":"demo"}},"serviceName":"demo",
                "template":{"metadata":{"labels":{"app":"demo"}}},
                "podManagementPolicy":"Parallel",
                "persistentVolumeClaimRetentionPolicy":{"whenDeleted":"Delete","whenScaled":"Retain"}}
        })).expect("desired");
        let mut initial = serde_json::to_value(&desired).expect("json");
        initial["metadata"]["uid"] = "old".into();
        initial["metadata"]["resourceVersion"] = "1".into();
        initial["spec"]["podManagementPolicy"] = "OrderedReady".into();
        // (live object, deleted controller count, injected failure, competing create)
        let state = Arc::new(Mutex::new((Some(initial), 0, false, false)));
        let shared = state.clone();
        let service = tower::service_fn(move |req: Request<kube::client::Body>| {
            let shared = shared.clone();
            async move {
                let method = req.method().clone();
                let bytes = req.into_body().collect().await.expect("body").to_bytes();
                let input: serde_json::Value = if bytes.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes).expect("json")
                };
                let mut state = shared.lock().expect("state");
                let error = |code| {
                    serde_json::json!({"apiVersion":"v1","kind":"Status",
                    "status":"Failure","reason":match code { 404 => "NotFound", 409 => "Conflict", _ => "InternalError" },"message":"injected","code":code})
                };
                let (code, value) = if method == Method::GET {
                    match &state.0 {
                        Some(v) => (200, v.clone()),
                        None => (404, error(404)),
                    }
                } else if method == Method::PATCH {
                    let live = state.0.as_mut().expect("live");
                    assert_eq!(input["metadata"]["uid"], live["metadata"]["uid"]);
                    assert_eq!(
                        input["metadata"]["resourceVersion"],
                        live["metadata"]["resourceVersion"]
                    );
                    if input["spec"]["podManagementPolicy"].is_null() {
                        live["spec"]["persistentVolumeClaimRetentionPolicy"] =
                            input["spec"]["persistentVolumeClaimRetentionPolicy"].clone();
                    } else {
                        live["spec"] = input["spec"].clone();
                    }
                    live["metadata"]["resourceVersion"] = "2".into();
                    (200, live.clone())
                } else if method == Method::DELETE {
                    let live = state.0.as_ref().expect("live");
                    assert_eq!(input["propagationPolicy"], "Orphan");
                    assert_eq!(input["preconditions"]["uid"], live["metadata"]["uid"]);
                    assert_eq!(
                        input["preconditions"]["resourceVersion"],
                        live["metadata"]["resourceVersion"]
                    );
                    assert_eq!(
                        live["spec"]["persistentVolumeClaimRetentionPolicy"]["whenDeleted"],
                        "Retain"
                    );
                    let deleted = live.clone();
                    state.0 = None;
                    state.1 += 1;
                    (200, deleted)
                } else if interrupt && !state.2 {
                    state.2 = true;
                    (500, error(500))
                } else if concurrent && !state.3 {
                    // A stale writer wins creation in the orphan-deletion gap.
                    let mut winner = input;
                    winner["metadata"]["uid"] = "competitor".into();
                    winner["metadata"]["resourceVersion"] = "1".into();
                    winner["spec"]["podManagementPolicy"] = "OrderedReady".into();
                    state.0 = Some(winner);
                    state.3 = true;
                    (409, error(409))
                } else {
                    assert!(
                        state.0.is_none(),
                        "creation must not overwrite a competing object"
                    );
                    let mut created = input;
                    created["metadata"]["uid"] = "new".into();
                    created["metadata"]["resourceVersion"] = "1".into();
                    state.0 = Some(created.clone());
                    (201, created)
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(Body::from(value.to_string()))
                        .expect("response"),
                )
            }
        });
        let api = Api::namespaced(kube::Client::new(service, "ns"), "ns");
        let first = apply_statefulset(&api, &desired, || async { Ok(()) }).await;
        if interrupt {
            assert!(first.is_err());
            assert!(state.lock().expect("state").0.is_none());
            apply_statefulset(&api, &desired, || async { Ok(()) })
                .await
                .expect("resume orphaned migration");
        } else {
            first.expect("migrate");
        }
        apply_statefulset(&api, &desired, || async { Ok(()) })
            .await
            .expect("idempotent retry");
        let mut stale = desired.clone();
        stale.spec.as_mut().expect("spec").pod_management_policy = None;
        assert!(
            apply_statefulset(&api, &stale, || async { Ok(()) })
                .await
                .is_err(),
            "stale writer must not undo migration"
        );
        let state = state.lock().expect("state");
        assert_eq!(
            state.0.as_ref().expect("live")["spec"]["podManagementPolicy"],
            "Parallel"
        );
        assert_eq!(state.1, if concurrent { 2 } else { 1 });
    }

    #[tokio::test]
    async fn resumes_after_interruption_between_orphan_delete_and_create() {
        migration(true, false).await;
    }

    #[tokio::test]
    async fn converges_after_stale_concurrent_creator_without_reversing_parallel() {
        migration(false, true).await;
    }
}
