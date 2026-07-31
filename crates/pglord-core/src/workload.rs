//! How cluster instances are scheduled and resourced.
//!
//! Defaults are chosen to work on a one-node dev cluster and still do the right
//! thing on a real one: anti-affinity is *preferred*, so instances spread
//! across nodes when nodes exist and schedule anyway when they do not.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Label and annotation prefixes pglord owns. A user value here could shadow
/// the labels the StatefulSet selector and the -rw service match on.
pub const RESERVED_KEY_PREFIXES: &[&str] = &[
    "app.kubernetes.io/",
    "pglord.dev/",
    "statefulset.kubernetes.io/",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AntiAffinity {
    /// Spread instances across nodes when possible (default).
    #[default]
    Preferred,
    /// Never place two instances on one node, even if that leaves pods pending.
    Required,
    Disabled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<String>,
}

impl Resources {
    pub fn is_empty(&self) -> bool {
        self.cpu_request.is_none()
            && self.cpu_limit.is_none()
            && self.memory_request.is_none()
            && self.memory_limit.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Toleration {
    pub key: String,
    /// "Equal" (default) or "Exists".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// "NoSchedule", "PreferNoSchedule" or "NoExecute".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WorkloadSpec {
    /// Extra labels on instance pods (cost allocation, scrape selection).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default)]
    pub resources: Resources,
    /// StorageClass for instance volumes. Immutable after creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default)]
    pub tolerations: Vec<Toleration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_class_name: Option<String>,
    #[serde(default)]
    pub anti_affinity: AntiAffinity,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorkloadError {
    #[error("'{0}' is reserved by pglord and cannot be set as a label or annotation")]
    ReservedKey(String),
    #[error("invalid key '{0}': must not be empty")]
    InvalidKey(String),
    #[error("invalid {field} '{value}': expected a quantity like 500m, 2 or 1Gi")]
    InvalidQuantity { field: String, value: String },
    #[error("toleration {0} requires a key")]
    TolerationWithoutKey(usize),
    #[error("storage class cannot be changed after the cluster is created ({from} -> {to})")]
    StorageClassImmutable { from: String, to: String },
}

pub fn is_reserved_key(key: &str) -> bool {
    RESERVED_KEY_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// Accepts Kubernetes quantity shapes we care about: plain numbers, milli-CPU,
/// and binary sizes. Kubernetes validates the rest authoritatively.
pub fn is_quantity(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if digits.is_empty() || digits.parse::<f64>().is_err() {
        return false;
    }
    matches!(
        &s[digits.len()..],
        "" | "m" | "k" | "M" | "G" | "T" | "Ki" | "Mi" | "Gi" | "Ti"
    )
}

pub fn validate(w: &WorkloadSpec) -> Result<(), WorkloadError> {
    for key in w.labels.keys().chain(w.annotations.keys()) {
        if key.is_empty() {
            return Err(WorkloadError::InvalidKey(key.clone()));
        }
        if is_reserved_key(key) {
            return Err(WorkloadError::ReservedKey(key.clone()));
        }
    }
    for key in w.node_selector.keys() {
        if key.is_empty() {
            return Err(WorkloadError::InvalidKey(key.clone()));
        }
    }
    let checks = [
        ("cpu_request", &w.resources.cpu_request),
        ("cpu_limit", &w.resources.cpu_limit),
        ("memory_request", &w.resources.memory_request),
        ("memory_limit", &w.resources.memory_limit),
    ];
    for (field, value) in checks {
        if let Some(v) = value
            && !is_quantity(v)
        {
            return Err(WorkloadError::InvalidQuantity {
                field: field.to_string(),
                value: v.clone(),
            });
        }
    }
    for (i, t) in w.tolerations.iter().enumerate() {
        if t.key.trim().is_empty() {
            return Err(WorkloadError::TolerationWithoutKey(i));
        }
    }
    Ok(())
}

/// StatefulSet volumeClaimTemplates are immutable, so the StorageClass can only
/// be chosen at creation.
pub fn check_storage_class_change(
    current: &WorkloadSpec,
    next: &WorkloadSpec,
) -> Result<(), WorkloadError> {
    if current.storage_class != next.storage_class {
        return Err(WorkloadError::StorageClassImmutable {
            from: current
                .storage_class
                .clone()
                .unwrap_or_else(|| "default".into()),
            to: next
                .storage_class
                .clone()
                .unwrap_or_else(|| "default".into()),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_spread_instances_softly() {
        // A required rule would leave a one-node cluster permanently Pending.
        assert_eq!(
            WorkloadSpec::default().anti_affinity,
            AntiAffinity::Preferred
        );
        assert!(validate(&WorkloadSpec::default()).is_ok());
    }

    #[test]
    fn reserved_keys_are_rejected() {
        for key in [
            "app.kubernetes.io/instance",
            "pglord.dev/kind",
            "statefulset.kubernetes.io/pod-name",
        ] {
            let w = WorkloadSpec {
                labels: BTreeMap::from([(key.to_string(), "x".to_string())]),
                ..Default::default()
            };
            assert!(
                matches!(validate(&w), Err(WorkloadError::ReservedKey(_))),
                "expected {key} to be rejected"
            );
        }
        // A normal key is fine.
        let w = WorkloadSpec {
            labels: BTreeMap::from([("team".into(), "payments".into())]),
            ..Default::default()
        };
        assert!(validate(&w).is_ok());
    }

    #[test]
    fn quantities_are_shape_checked() {
        for good in ["500m", "2", "1.5", "1Gi", "512Mi", "100M"] {
            assert!(is_quantity(good), "{good} should be valid");
        }
        for bad in ["", "lots", "1 Gi", "Gi", "-1", "1x"] {
            assert!(!is_quantity(bad), "{bad} should be invalid");
        }

        let w = WorkloadSpec {
            resources: Resources {
                memory_request: Some("512 megs".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            validate(&w),
            Err(WorkloadError::InvalidQuantity { .. })
        ));
    }

    #[test]
    fn storage_class_is_immutable() {
        let current = WorkloadSpec {
            storage_class: Some("fast".into()),
            ..Default::default()
        };
        let same = current.clone();
        assert!(check_storage_class_change(&current, &same).is_ok());

        let changed = WorkloadSpec {
            storage_class: Some("slow".into()),
            ..Default::default()
        };
        assert!(matches!(
            check_storage_class_change(&current, &changed),
            Err(WorkloadError::StorageClassImmutable { .. })
        ));
    }

    #[test]
    fn tolerations_need_a_key() {
        let w = WorkloadSpec {
            tolerations: vec![Toleration {
                key: "  ".into(),
                operator: None,
                value: None,
                effect: None,
            }],
            ..Default::default()
        };
        assert!(matches!(
            validate(&w),
            Err(WorkloadError::TolerationWithoutKey(0))
        ));
    }
}
