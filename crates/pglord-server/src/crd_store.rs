//! SpecStore over PostgresCluster CRs. The CR is the truth; the primary
//! record stays on the mirror ConfigMap (engine state, written by the CR
//! sync loop), so the failover CAS path is byte-identical in both modes.

use kube::api::{Api, Patch, PatchParams};
use pglord_core::crd::PostgresCluster;
use pglord_core::render::OwnerRef;
use pglord_core::spec::ClusterSpec;

use crate::spec_store::{ConfigMapSpecStore, FIELD_MANAGER, SpecStore};

pub struct CrdSpecStore {
    client: kube::Client,
    own_ns: String,
    all_namespaces: bool,
    /// Delegation target for the primary record on the mirror ConfigMap.
    mirror: ConfigMapSpecStore,
}

impl CrdSpecStore {
    pub fn new(client: kube::Client, own_ns: &str, all_namespaces: bool) -> CrdSpecStore {
        CrdSpecStore {
            mirror: ConfigMapSpecStore::new(client.clone(), own_ns),
            own_ns: own_ns.to_string(),
            all_namespaces,
            client,
        }
    }

    fn api(&self, ns: &str) -> Api<PostgresCluster> {
        Api::namespaced(self.client.clone(), ns)
    }
}

/// The spec as the engine sees it: metadata.name wins when spec.name is
/// empty. A non-empty mismatch is left as-is — flagging it is the CR sync's
/// job, and the API's get-by-name simply won't find a mismatched spec.
pub fn effective_spec(cr: &PostgresCluster) -> ClusterSpec {
    let mut spec = cr.spec.clone();
    if spec.name.is_empty()
        && let Some(name) = &cr.metadata.name
    {
        spec.name.clone_from(name);
    }
    spec
}

pub fn cr_owner_ref(cr: &PostgresCluster) -> OwnerRef {
    OwnerRef {
        api_version: "pglord.dev/v1alpha1".into(),
        kind: "PostgresCluster".into(),
        name: cr.metadata.name.clone().unwrap_or_default(),
        uid: cr.metadata.uid.clone().unwrap_or_default(),
    }
}

#[async_trait::async_trait]
impl SpecStore for CrdSpecStore {
    async fn list_all(&self) -> anyhow::Result<Vec<(String, ClusterSpec)>> {
        let crs = if self.all_namespaces {
            Api::<PostgresCluster>::all(self.client.clone())
                .list(&Default::default())
                .await?
        } else {
            self.api(&self.own_ns).list(&Default::default()).await?
        };
        Ok(crs
            .items
            .iter()
            .map(|cr| {
                (
                    cr.metadata
                        .namespace
                        .clone()
                        .unwrap_or_else(|| self.own_ns.clone()),
                    effective_spec(cr),
                )
            })
            .collect())
    }

    async fn get(&self, ns: &str, name: &str) -> anyhow::Result<Option<(ClusterSpec, OwnerRef)>> {
        Ok(self
            .api(ns)
            .get_opt(name)
            .await?
            .map(|cr| (effective_spec(&cr), cr_owner_ref(&cr))))
    }

    async fn put(&self, ns: &str, spec: &ClusterSpec) -> anyhow::Result<()> {
        let cr = PostgresCluster::new(&spec.name, spec.clone());
        self.api(ns)
            .patch(
                &spec.name,
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(&cr),
            )
            .await?;
        Ok(())
    }

    async fn delete(&self, ns: &str, name: &str) -> anyhow::Result<bool> {
        match self.api(ns).delete(name, &Default::default()).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn primary(&self, ns: &str, name: &str) -> anyhow::Result<Option<String>> {
        self.mirror.primary(ns, name).await
    }

    async fn set_primary(&self, ns: &str, name: &str, target: &str) -> anyhow::Result<bool> {
        self.mirror.set_primary(ns, name, target).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_name_fills_an_empty_spec_name() {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "" })).expect("spec");
        let mut cr = PostgresCluster::new("demo", spec);
        assert_eq!(effective_spec(&cr).name, "demo");
        // A non-empty mismatch is preserved; the sync loop flags it.
        cr.spec.name = "other".into();
        assert_eq!(effective_spec(&cr).name, "other");
    }
}
