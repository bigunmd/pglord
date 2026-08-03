//! Operator-mode CRD surface. The spec type is ClusterSpec verbatim
//! (spec.rs carries the CustomResource derive, so the CRD schema and the
//! REST contract can never drift apart); this module holds the status
//! subresource and re-exports the generated custom resource type.

pub use crate::spec::PostgresCluster;

#[derive(
    Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct PostgresClusterStatus {
    /// Instances currently passing their readiness probe.
    #[serde(default)]
    pub ready_replicas: i32,
    /// Recorded primary pod.
    #[serde(default)]
    pub primary: String,
    /// Scaled to zero on purpose; volumes retained.
    #[serde(default)]
    pub hibernated: bool,
    /// Progressing | Ready | Degraded | Hibernated | Invalid.
    #[serde(default)]
    pub phase: String,
    /// Human-readable detail for Invalid/Degraded; empty otherwise.
    #[serde(default)]
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn crd_manifest_has_expected_identity() {
        let crd = PostgresCluster::crd();
        assert_eq!(crd.spec.group, "pglord.dev");
        assert_eq!(crd.spec.names.kind, "PostgresCluster");
        assert_eq!(crd.spec.names.short_names, Some(vec!["pgc".to_string()]));
        let v = &crd.spec.versions[0];
        assert_eq!(v.name, "v1alpha1");
        assert!(
            v.subresources
                .as_ref()
                .and_then(|s| s.status.as_ref())
                .is_some()
        );
        let cols = v
            .additional_printer_columns
            .as_ref()
            .expect("printer columns");
        assert!(cols.iter().any(|c| c.json_path == ".status.ready_replicas"));
    }

    #[test]
    fn spec_schema_uses_snake_case_verbatim() {
        let crd = PostgresCluster::crd();
        let schema = serde_json::to_string(&crd.spec.versions[0].schema).expect("schema json");
        assert!(schema.contains("storage_size"));
        assert!(schema.contains("pg_hba"));
    }
}
