//! Cluster specification: the user-facing desired state. These types later
//! become the CRD schema verbatim in operator mode, so evolve them carefully.

use serde::{Deserialize, Serialize};

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
}

impl ClusterSpec {
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
