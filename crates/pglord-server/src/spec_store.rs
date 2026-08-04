//! Cluster spec persistence. Standalone mode stores specs as labeled
//! ConfigMaps in the namespace; operator mode will implement this same trait
//! over CRDs without touching the reconciler or API layer.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, ListParams, Patch, PatchParams};
use pglord_core::render::OwnerRef;
use pglord_core::spec::ClusterSpec;

pub const KIND_LABEL: &str = "pglord.dev/kind";
pub const KIND_CLUSTER_SPEC: &str = "cluster-spec";
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
pub const SPEC_KEY: &str = "spec";
pub const FIELD_MANAGER: &str = "pglord";

pub fn spec_cm_name(cluster: &str) -> String {
    format!("{cluster}-spec")
}

pub fn spec_label_selector() -> String {
    format!("{MANAGED_BY_LABEL}=pglord,{KIND_LABEL}={KIND_CLUSTER_SPEC}")
}

/// Build the ConfigMap that carries a cluster spec.
pub fn spec_to_configmap(spec: &ClusterSpec, ns: &str) -> anyhow::Result<ConfigMap> {
    Ok(ConfigMap {
        metadata: ObjectMeta {
            name: Some(spec_cm_name(&spec.name)),
            namespace: Some(ns.to_string()),
            labels: Some(BTreeMap::from([
                (MANAGED_BY_LABEL.to_string(), "pglord".to_string()),
                (KIND_LABEL.to_string(), KIND_CLUSTER_SPEC.to_string()),
            ])),
            ..Default::default()
        },
        data: Some(BTreeMap::from([(
            SPEC_KEY.to_string(),
            serde_json::to_string(spec)?,
        )])),
        ..Default::default()
    })
}

/// The mirror ConfigMap operator mode derives from a PostgresCluster: the
/// standalone spec ConfigMap plus an ownerReference to the CR (GC chains
/// CR -> mirror -> children) and an "internal" label for humans. It carries
/// no annotations on purpose — the primary-record CAS owns those fields
/// under server-side apply and the sync must never wrestle them away.
pub fn spec_to_configmap_owned(
    spec: &ClusterSpec,
    ns: &str,
    owner: &OwnerRef,
) -> anyhow::Result<ConfigMap> {
    let mut cm = spec_to_configmap(spec, ns)?;
    if let Some(labels) = cm.metadata.labels.as_mut() {
        labels.insert("pglord.dev/internal".to_string(), "true".to_string());
    }
    cm.metadata.owner_references = Some(vec![
        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: owner.api_version.clone(),
            kind: owner.kind.clone(),
            name: owner.name.clone(),
            uid: owner.uid.clone(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        },
    ]);
    Ok(cm)
}

/// The in-flight major upgrade recorded on a spec ConfigMap, if any.
/// Unparseable state answers None with a warning rather than failing the
/// reconcile pass: a cluster must keep converging even if this annotation is
/// somehow corrupt.
pub fn upgrade_from_cm(cm: &ConfigMap) -> Option<pglord_core::upgrade::UpgradeState> {
    let raw = cm
        .metadata
        .annotations
        .as_ref()?
        .get(pglord_core::upgrade::UPGRADE_ANNOTATION)?;
    match serde_json::from_str(raw) {
        Ok(state) => Some(state),
        Err(err) => {
            tracing::warn!(cm = ?cm.metadata.name, %err, "ignoring unparseable upgrade state");
            None
        }
    }
}

/// Parse a spec ConfigMap back into a ClusterSpec + the OwnerRef children use.
pub fn configmap_to_spec(cm: &ConfigMap) -> anyhow::Result<(ClusterSpec, OwnerRef)> {
    let data = cm
        .data
        .as_ref()
        .and_then(|d| d.get(SPEC_KEY))
        .ok_or_else(|| anyhow::anyhow!("spec ConfigMap missing '{SPEC_KEY}' key"))?;
    let spec: ClusterSpec = serde_json::from_str(data)?;
    let owner = OwnerRef {
        api_version: "v1".to_string(),
        kind: "ConfigMap".to_string(),
        name: cm.metadata.name.clone().unwrap_or_default(),
        uid: cm.metadata.uid.clone().unwrap_or_default(),
    };
    Ok((spec, owner))
}

#[async_trait::async_trait]
pub trait SpecStore: Send + Sync {
    /// Every cluster this store manages, with the namespace it lives in.
    async fn list_all(&self) -> anyhow::Result<Vec<(String, ClusterSpec)>>;
    async fn get(&self, ns: &str, name: &str) -> anyhow::Result<Option<(ClusterSpec, OwnerRef)>>;
    async fn put(&self, ns: &str, spec: &ClusterSpec) -> anyhow::Result<()>;
    async fn delete(&self, ns: &str, name: &str) -> anyhow::Result<bool>;
    /// The recorded primary pod for the cluster, if the spec exists
    /// (annotation on the spec object; caller applies the `<name>-0` default).
    async fn primary(&self, ns: &str, name: &str) -> anyhow::Result<Option<String>>;
    /// Record a new primary via compare-and-swap. False = another writer won
    /// the race; the caller should re-read and decide again.
    async fn set_primary(&self, ns: &str, name: &str, target: &str) -> anyhow::Result<bool>;
    /// The in-flight major upgrade, if one is running.
    async fn upgrade_state(
        &self,
        ns: &str,
        name: &str,
    ) -> anyhow::Result<Option<pglord_core::upgrade::UpgradeState>>;
    /// CAS-write the upgrade state (None clears it), optionally flipping the
    /// primary record in the same patch — cutover and rollback need both to
    /// move atomically. False = lost the race; re-read and decide again.
    async fn set_upgrade(
        &self,
        ns: &str,
        name: &str,
        state: Option<&pglord_core::upgrade::UpgradeState>,
        set_primary_to: Option<&str>,
    ) -> anyhow::Result<bool>;
}

pub struct ConfigMapSpecStore {
    client: kube::Client,
    /// Standalone mode is single-namespace by design; list_all only ever
    /// reports this namespace.
    own_ns: String,
}

impl ConfigMapSpecStore {
    pub fn new(client: kube::Client, own_ns: &str) -> Self {
        ConfigMapSpecStore {
            client,
            own_ns: own_ns.to_string(),
        }
    }

    fn api(&self, ns: &str) -> Api<ConfigMap> {
        Api::namespaced(self.client.clone(), ns)
    }
}

#[async_trait::async_trait]
impl SpecStore for ConfigMapSpecStore {
    async fn list_all(&self) -> anyhow::Result<Vec<(String, ClusterSpec)>> {
        let cms = self
            .api(&self.own_ns)
            .list(&ListParams::default().labels(&spec_label_selector()))
            .await?;
        Ok(cms
            .items
            .iter()
            .filter_map(|cm| configmap_to_spec(cm).ok())
            .map(|(spec, _)| (self.own_ns.clone(), spec))
            .collect())
    }

    async fn get(&self, ns: &str, name: &str) -> anyhow::Result<Option<(ClusterSpec, OwnerRef)>> {
        match self.api(ns).get_opt(&spec_cm_name(name)).await? {
            Some(cm) => Ok(Some(configmap_to_spec(&cm)?)),
            None => Ok(None),
        }
    }

    async fn put(&self, ns: &str, spec: &ClusterSpec) -> anyhow::Result<()> {
        let cm = spec_to_configmap(spec, ns)?;
        self.api(ns)
            .patch(
                &spec_cm_name(&spec.name),
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(&cm),
            )
            .await?;
        Ok(())
    }

    async fn delete(&self, ns: &str, name: &str) -> anyhow::Result<bool> {
        match self
            .api(ns)
            .delete(&spec_cm_name(name), &Default::default())
            .await
        {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn set_primary(&self, ns: &str, name: &str, target: &str) -> anyhow::Result<bool> {
        let api = self.api(ns);
        let cm_name = spec_cm_name(name);
        let Some(cm) = api.get_opt(&cm_name).await? else {
            return Ok(false);
        };
        let rv = cm.metadata.resource_version.unwrap_or_default();
        crate::primary_record::cas_set_primary(&api, &cm_name, &rv, target).await
    }

    async fn upgrade_state(
        &self,
        ns: &str,
        name: &str,
    ) -> anyhow::Result<Option<pglord_core::upgrade::UpgradeState>> {
        Ok(self
            .api(ns)
            .get_opt(&spec_cm_name(name))
            .await?
            .as_ref()
            .and_then(upgrade_from_cm))
    }

    async fn set_upgrade(
        &self,
        ns: &str,
        name: &str,
        state: Option<&pglord_core::upgrade::UpgradeState>,
        set_primary_to: Option<&str>,
    ) -> anyhow::Result<bool> {
        let api = self.api(ns);
        let cm_name = spec_cm_name(name);
        let Some(cm) = api.get_opt(&cm_name).await? else {
            return Ok(false);
        };
        let rv = cm.metadata.resource_version.unwrap_or_default();
        let mut annotations = serde_json::Map::new();
        annotations.insert(
            pglord_core::upgrade::UPGRADE_ANNOTATION.to_string(),
            match state {
                // A merge patch removes a key by setting it to null.
                None => serde_json::Value::Null,
                Some(s) => serde_json::Value::String(serde_json::to_string(s)?),
            },
        );
        if let Some(primary) = set_primary_to {
            annotations.insert(
                pglord_core::PRIMARY_ANNOTATION.to_string(),
                serde_json::Value::String(primary.to_string()),
            );
        }
        crate::primary_record::cas_set_annotations(&api, &cm_name, &rv, annotations).await
    }

    async fn primary(&self, ns: &str, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.api(ns).get_opt(&spec_cm_name(name)).await?.map(|cm| {
            // The bootstrap default is the *spec's* ordinal zero: after a
            // major upgrade that is "<name>-g2-0", not "<name>-0".
            let default_pod = configmap_to_spec(&cm)
                .map(|(s, _)| s.instance_name(0))
                .unwrap_or_else(|_| format!("{name}-0"));
            crate::primary_record::recorded_primary(&cm, &default_pod)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_roundtrips_through_configmap() {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "demo", "replicas": 3 }))
                .expect("spec");
        let mut cm = spec_to_configmap(&spec, "pglord").expect("cm");
        cm.metadata.uid = Some("uid-42".into());
        let (parsed, owner) = configmap_to_spec(&cm).expect("parse");
        assert_eq!(parsed, spec);
        assert_eq!(owner.name, "demo-spec");
        assert_eq!(owner.uid, "uid-42");
        assert_eq!(owner.kind, "ConfigMap");
    }

    #[test]
    fn upgrade_state_parses_from_the_annotation() {
        use pglord_core::upgrade::{UPGRADE_ANNOTATION, UpgradePhase};
        let mut cm = ConfigMap::default();
        assert!(upgrade_from_cm(&cm).is_none());
        cm.metadata.annotations = Some(
            [(
                UPGRADE_ANNOTATION.to_string(),
                r#"{"target_image":"a:18","next_generation":"g2","phase":"migrating"}"#.to_string(),
            )]
            .into(),
        );
        let st = upgrade_from_cm(&cm).expect("state");
        assert_eq!(st.phase, UpgradePhase::Migrating);
        assert_eq!(st.next_generation, "g2");
        assert_eq!(st.previous_primary, None);

        // Garbage must not take the reconcile pass down with it.
        cm.metadata.annotations = Some([(UPGRADE_ANNOTATION.to_string(), "{".to_string())].into());
        assert!(upgrade_from_cm(&cm).is_none());
    }

    #[test]
    fn configmap_without_spec_key_is_rejected() {
        let cm = ConfigMap::default();
        assert!(configmap_to_spec(&cm).is_err());
    }

    #[test]
    fn label_selector_matches_written_labels() {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "demo" })).expect("spec");
        let cm = spec_to_configmap(&spec, "ns").expect("cm");
        let labels = cm.metadata.labels.expect("labels");
        for pair in spec_label_selector().split(',') {
            let (k, v) = pair.split_once('=').expect("k=v");
            assert_eq!(labels.get(k).map(String::as_str), Some(v));
        }
    }
}
