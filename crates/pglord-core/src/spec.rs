//! Cluster specification: the user-facing desired state. These types later
//! become the CRD schema verbatim in operator mode, so evolve them carefully.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::guc;

const MAX_NAME_LEN: usize = 40; // leaves room for "-pods"/"-rw"/ordinal suffixes
const MAX_REPLICAS: u8 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct ClusterSpec {
    /// RFC1123 label; used as the prefix of every derived object name.
    pub name: String,
    /// Image containing postgres + pglord-agent (agent is the entrypoint).
    #[serde(default = "default_image")]
    pub image: String,
    /// Total instances: 1 primary + (replicas - 1) streaming standbys.
    #[serde(default = "default_replicas")]
    pub replicas: u8,
    /// PVC size per instance, e.g. "1Gi".
    #[serde(default = "default_storage_size")]
    pub storage_size: String,
    /// PostgreSQL settings applied on top of the defaults. Only settings in the
    /// GUC catalog are accepted; pglord-managed settings are rejected.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
}

fn default_image() -> String {
    "pglord-agent:dev".into()
}
fn default_replicas() -> u8 {
    1
}
fn default_storage_size() -> String {
    "1Gi".into()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpecError {
    #[error("invalid cluster name '{0}': must be an RFC1123 label of at most 40 chars")]
    InvalidName(String),
    #[error("invalid replicas {0}: must be between 1 and 5")]
    InvalidReplicas(u8),
    #[error("invalid storage size '{0}': expected e.g. '1Gi', '500Mi'")]
    InvalidStorage(String),
    #[error("storage cannot shrink: {from} -> {to}")]
    StorageShrink { from: String, to: String },
    #[error("unknown PostgreSQL setting '{0}'")]
    UnknownParameter(String),
    #[error("'{0}' is managed by pglord and cannot be overridden")]
    ReservedParameter(String),
    #[error("invalid value for '{name}': {reason}")]
    InvalidParameterValue { name: String, reason: String },
}

/// The fields a running cluster may change. Name and image are deliberately
/// absent: renaming would orphan every derived object, and image changes are a
/// rolling-upgrade concern with its own (future) workflow.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SpecChange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_size: Option<String>,
    /// Full replacement of the parameter map: settings absent from it are
    /// removed, which is how the UI clears a setting back to its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<BTreeMap<String, String>>,
}

/// Size in bytes for comparison; None when the quantity is malformed.
pub fn quantity_bytes(s: &str) -> Option<u64> {
    let idx = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(idx);
    let value: u64 = num.parse().ok()?;
    let multiplier: u64 = match unit {
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        _ => return None,
    };
    value.checked_mul(multiplier)
}

impl ClusterSpec {
    /// Apply a change to a running cluster's spec, returning the new spec.
    /// Kubernetes cannot shrink a PVC, so a smaller storage request is
    /// rejected here rather than failing later against the apiserver.
    pub fn apply_change(&self, change: &SpecChange) -> Result<ClusterSpec, SpecError> {
        let mut next = self.clone();
        if let Some(replicas) = change.replicas {
            next.replicas = replicas;
        }
        if let Some(storage) = &change.storage_size {
            let to = quantity_bytes(storage)
                .ok_or_else(|| SpecError::InvalidStorage(storage.clone()))?;
            let from = quantity_bytes(&self.storage_size)
                .ok_or_else(|| SpecError::InvalidStorage(self.storage_size.clone()))?;
            if to < from {
                return Err(SpecError::StorageShrink {
                    from: self.storage_size.clone(),
                    to: storage.clone(),
                });
            }
            next.storage_size = storage.clone();
        }
        if let Some(parameters) = &change.parameters {
            next.parameters = parameters.clone();
        }
        next.validate()?;
        Ok(next)
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        if !is_rfc1123_label(&self.name) || self.name.len() > MAX_NAME_LEN {
            return Err(SpecError::InvalidName(self.name.clone()));
        }
        if self.replicas < 1 || self.replicas > MAX_REPLICAS {
            return Err(SpecError::InvalidReplicas(self.replicas));
        }
        if !is_quantity(&self.storage_size) {
            return Err(SpecError::InvalidStorage(self.storage_size.clone()));
        }
        for (name, value) in &self.parameters {
            if guc::is_reserved(name) {
                return Err(SpecError::ReservedParameter(name.clone()));
            }
            let Some(g) = guc::lookup(name) else {
                return Err(SpecError::UnknownParameter(name.clone()));
            };
            guc::validate_value(g, value).map_err(|reason| SpecError::InvalidParameterValue {
                name: name.clone(),
                reason,
            })?;
        }
        Ok(())
    }
}

fn is_rfc1123_label(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn is_quantity(s: &str) -> bool {
    let Some(idx) = s.find(|c: char| !c.is_ascii_digit()) else {
        return false; // bare number: require an explicit unit
    };
    let (num, unit) = s.split_at(idx);
    !num.is_empty() && matches!(unit, "Ki" | "Mi" | "Gi" | "Ti")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ClusterSpec {
        serde_json::from_value(serde_json::json!({ "name": name })).expect("deserialize")
    }

    #[test]
    fn minimal_json_gets_defaults() {
        let s = spec("demo");
        assert_eq!(s.image, "pglord-agent:dev");
        assert_eq!(s.replicas, 1);
        assert_eq!(s.storage_size, "1Gi");
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn bad_names_rejected() {
        for bad in ["Demo", "de_mo", "-demo", "demo-", "", &"a".repeat(41)] {
            assert!(
                matches!(spec(bad).validate(), Err(SpecError::InvalidName(_))),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn replica_bounds_enforced() {
        let mut s = spec("demo");
        s.replicas = 0;
        assert_eq!(s.validate(), Err(SpecError::InvalidReplicas(0)));
        s.replicas = 6;
        assert_eq!(s.validate(), Err(SpecError::InvalidReplicas(6)));
        s.replicas = 5;
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn name_length_boundaries() {
        // 40 chars is the documented maximum (leaves room for -pods/-rw and
        // pod ordinals within the 63-char label limit).
        let mut s = spec(&"a".repeat(40));
        assert_eq!(s.validate(), Ok(()));
        s.name = "a".repeat(41);
        assert!(matches!(s.validate(), Err(SpecError::InvalidName(_))));
        s.name = "a".into();
        assert_eq!(s.validate(), Ok(()));
        s.name = "1".into();
        assert_eq!(s.validate(), Ok(()));
        s.name = "orders-db-2".into();
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn storage_units_are_case_sensitive() {
        let mut s = spec("demo");
        s.storage_size = "1gi".into();
        assert!(matches!(s.validate(), Err(SpecError::InvalidStorage(_))));
        s.storage_size = "1GI".into();
        assert!(matches!(s.validate(), Err(SpecError::InvalidStorage(_))));
        s.storage_size = "1Gi".into();
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn quantity_comparison_is_unit_aware() {
        assert_eq!(quantity_bytes("1Gi"), quantity_bytes("1024Mi"));
        assert!(quantity_bytes("2Gi") > quantity_bytes("1500Mi"));
        assert_eq!(quantity_bytes("1Ki"), Some(1024));
        for bad in ["1G", "abc", "Gi", "1", ""] {
            assert_eq!(quantity_bytes(bad), None, "expected None for {bad:?}");
        }
    }

    #[test]
    fn change_scales_replicas_within_bounds() {
        let s = spec("demo");
        let up = s
            .apply_change(&SpecChange {
                replicas: Some(3),
                ..Default::default()
            })
            .expect("scale up");
        assert_eq!(up.replicas, 3);
        assert_eq!(up.storage_size, s.storage_size);

        assert!(matches!(
            s.apply_change(&SpecChange {
                replicas: Some(9),
                ..Default::default()
            }),
            Err(SpecError::InvalidReplicas(9))
        ));
    }

    #[test]
    fn storage_may_grow_but_never_shrink() {
        let s = spec("demo"); // 1Gi
        let grown = s
            .apply_change(&SpecChange {
                storage_size: Some("5Gi".into()),
                ..Default::default()
            })
            .expect("grow");
        assert_eq!(grown.storage_size, "5Gi");

        // Same size expressed in different units is not a shrink.
        assert!(
            s.apply_change(&SpecChange {
                storage_size: Some("1024Mi".into()),
                ..Default::default()
            })
            .is_ok()
        );

        assert!(matches!(
            grown.apply_change(&SpecChange {
                storage_size: Some("1Gi".into()),
                ..Default::default()
            }),
            Err(SpecError::StorageShrink { .. })
        ));
    }

    #[test]
    fn parameters_are_validated_against_the_catalog() {
        let mut s = spec("demo");
        s.parameters = BTreeMap::from([("work_mem".into(), "16MB".into())]);
        assert_eq!(s.validate(), Ok(()));

        s.parameters = BTreeMap::from([("wrk_mem".into(), "16MB".into())]);
        assert!(matches!(s.validate(), Err(SpecError::UnknownParameter(_))));

        // Overriding a managed setting would break replication or the agent's
        // ability to reach postgres.
        s.parameters = BTreeMap::from([("wal_level".into(), "minimal".into())]);
        assert!(matches!(s.validate(), Err(SpecError::ReservedParameter(_))));

        s.parameters = BTreeMap::from([("max_connections".into(), "many".into())]);
        assert!(matches!(
            s.validate(),
            Err(SpecError::InvalidParameterValue { .. })
        ));
    }

    #[test]
    fn parameter_change_replaces_the_whole_map() {
        let mut s = spec("demo");
        s.parameters = BTreeMap::from([
            ("work_mem".into(), "16MB".into()),
            ("log_connections".into(), "on".into()),
        ]);
        let next = s
            .apply_change(&SpecChange {
                parameters: Some(BTreeMap::from([("work_mem".into(), "32MB".into())])),
                ..Default::default()
            })
            .expect("change");
        // log_connections is gone: absent from the new map means "back to default".
        assert_eq!(next.parameters.len(), 1);
        assert_eq!(next.parameters["work_mem"], "32MB");

        // Changing only replicas leaves parameters untouched.
        let scaled = s
            .apply_change(&SpecChange {
                replicas: Some(3),
                ..Default::default()
            })
            .expect("change");
        assert_eq!(scaled.parameters, s.parameters);
    }

    #[test]
    fn empty_change_is_a_noop() {
        let s = spec("demo");
        let same = s.apply_change(&SpecChange::default()).expect("noop");
        assert_eq!(same, s);
    }

    #[test]
    fn unknown_fields_are_tolerated_for_forward_compatibility() {
        // A spec written by a newer pglord must still parse here, otherwise a
        // rollback would strand every existing cluster.
        let s: ClusterSpec = serde_json::from_value(serde_json::json!({
            "name": "demo",
            "someFutureField": {"a": 1}
        }))
        .expect("deserialize with unknown field");
        assert_eq!(s.name, "demo");
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn storage_quantities_validated() {
        for good in ["1Gi", "500Mi", "10Ti", "512Ki"] {
            let mut s = spec("demo");
            s.storage_size = good.into();
            assert_eq!(s.validate(), Ok(()), "expected ok for {good}");
        }
        for bad in ["abc", "1G", "Gi", "1", ""] {
            let mut s = spec("demo");
            s.storage_size = bad.into();
            assert!(
                matches!(s.validate(), Err(SpecError::InvalidStorage(_))),
                "expected rejection for {bad:?}"
            );
        }
    }
}
