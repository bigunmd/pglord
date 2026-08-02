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
    /// Every commit waits until any one standby confirms the WAL flush
    /// (quorum `ANY 1` over all standbys). Strict: with no standby connected,
    /// writes block rather than silently falling back to async. Requires
    /// `replicas >= 2`. Applied with a reload — no restart.
    #[serde(default)]
    pub synchronous: bool,
    /// PostgreSQL settings applied on top of the defaults. Only settings in the
    /// GUC catalog are accepted; pglord-managed settings are rejected.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    /// Client access rules (pg_hba.conf) inserted between pglord's managed
    /// prefix and its scram catch-all. First match wins; use `reject` rules
    /// to tighten access. Applied with a reload — no restart.
    #[serde(default)]
    pub pg_hba: Vec<crate::hba::HbaRule>,
    /// Scheduling, resources and Kubernetes metadata for the instances.
    #[serde(default)]
    pub workload: crate::workload::WorkloadSpec,
    /// Backup repository. Disabled by default; enabling it also turns on WAL
    /// archiving, which needs a restart.
    #[serde(default)]
    pub backup: crate::backup::BackupSpec,
    /// Where this cluster's data came from, when it was created by restoring
    /// someone's backup. A one-shot bootstrap source: it is consumed the first
    /// time PGDATA is created and has no effect on a running cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<crate::restore::RestoreSpec>,
    /// Secret holding the superuser and replication passwords. When set,
    /// pglord uses it instead of generating one — so the cluster has a single
    /// source of truth for its credentials rather than two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_secret: Option<String>,
    /// Extra environment variables on the instance containers, for init
    /// scripts and anything else in the container to read.
    #[serde(default)]
    pub env: Vec<crate::env::EnvVarSpec>,
    /// SQL run once, when the data directory is first created.
    #[serde(default)]
    pub init: crate::init::InitSpec,
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
    #[error("synchronous replication requires at least 2 replicas")]
    SyncRequiresReplicas,
    #[error(
        "synchronous replication conflicts with synchronous_commit = '{0}'; \
         use 'on', 'remote_write' or 'remote_apply'"
    )]
    SyncConflictsSynchronousCommit(String),
    #[error("{0}")]
    InvalidHba(#[from] crate::hba::HbaError),
    #[error("{0}")]
    InvalidWorkload(#[from] crate::workload::WorkloadError),
    #[error("{0}")]
    InvalidBackup(#[from] crate::backup::BackupError),
    #[error("{0}")]
    InvalidRestore(#[from] crate::restore::RestoreError),
    #[error("{0}")]
    InvalidEnv(#[from] crate::env::EnvError),
    #[error("{0}")]
    InvalidInit(#[from] crate::init::InitError),
    #[error("invalid credentials secret name '{0}': must be a lowercase RFC1123 name")]
    InvalidCredentialsSecret(String),
}

/// The fields a running cluster may change. Name and image are deliberately
/// absent: renaming would orphan every derived object, and image changes are a
/// rolling-upgrade concern with its own (future) workflow.
///
/// `credentials_secret` is absent for a subtler reason. Pointing a running
/// cluster at a different Secret would change the password the agent presents
/// without changing the one stored in `pg_authid`, so every client using the
/// new Secret would be rejected while the cluster looked healthy. Rotating
/// credentials is a real operation and needs its own workflow, not a field
/// edit that silently half-applies.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SpecChange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_size: Option<String>,
    /// Toggle synchronous replication. Reload-only: no restart, no roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronous: Option<bool>,
    /// Full replacement of the parameter map: settings absent from it are
    /// removed, which is how the UI clears a setting back to its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<BTreeMap<String, String>>,
    /// Full replacement of the pg_hba rule list: absent leaves it untouched,
    /// an empty list removes every user rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_hba: Option<Vec<crate::hba::HbaRule>>,
    /// Full replacement of the workload block. StorageClass must match the
    /// current one (volume claim templates are immutable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<crate::workload::WorkloadSpec>,
    /// Full replacement of the backup block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<crate::backup::BackupSpec>,
    /// Full replacement of the custom environment. Changing it rolls the
    /// instances, since the pod template changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<crate::env::EnvVarSpec>>,
    /// Full replacement of the init block. Editing this does *not* re-run
    /// initialisation: scripts describe how a database is created, and the
    /// cluster's data directory already exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<crate::init::InitSpec>,
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
        if let Some(synchronous) = change.synchronous {
            next.synchronous = synchronous;
        }
        if let Some(parameters) = &change.parameters {
            next.parameters = parameters.clone();
        }
        if let Some(pg_hba) = &change.pg_hba {
            next.pg_hba = pg_hba.clone();
        }
        if let Some(workload) = &change.workload {
            crate::workload::check_storage_class_change(&self.workload, workload)?;
            next.workload = workload.clone();
        }
        if let Some(backup) = &change.backup {
            next.backup = backup.clone();
        }
        if let Some(env) = &change.env {
            next.env = env.clone();
        }
        if let Some(init) = &change.init {
            next.init = init.clone();
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
        if self.synchronous {
            if self.replicas < 2 {
                return Err(SpecError::SyncRequiresReplicas);
            }
            if let Some(v) = self.parameters.get("synchronous_commit")
                && matches!(v.as_str(), "off" | "local")
            {
                return Err(SpecError::SyncConflictsSynchronousCommit(v.clone()));
            }
        }
        crate::hba::validate_rules(&self.pg_hba)?;
        crate::workload::validate(&self.workload)?;
        crate::backup::validate(&self.backup)?;
        if let Some(restore) = &self.restore {
            crate::restore::validate(restore)?;
        }
        if let Some(secret) = self.credentials_secret()
            && !is_rfc1123_label(secret)
        {
            return Err(SpecError::InvalidCredentialsSecret(secret.to_string()));
        }
        crate::env::validate(&self.env)?;
        crate::init::validate(&self.init)?;
        Ok(())
    }

    /// The user-supplied credentials Secret, ignoring blanks a form might send.
    /// `None` means pglord generates and owns the cluster's credentials.
    pub fn credentials_secret(&self) -> Option<&str> {
        self.credentials_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
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
    fn environment_and_init_can_be_replaced_wholesale() {
        let mut s = spec("demo");
        s.env = serde_json::from_value(serde_json::json!([
            { "name": "A", "value": "1" },
            { "name": "B", "value": "2" }
        ]))
        .expect("env");
        let next = s
            .apply_change(&SpecChange {
                env: Some(
                    serde_json::from_value(serde_json::json!([{ "name": "A", "value": "9" }]))
                        .expect("env"),
                ),
                init: Some(
                    serde_json::from_value(serde_json::json!({ "scripts_config_map": "init-sql" }))
                        .expect("init"),
                ),
                ..Default::default()
            })
            .expect("change");
        // Absent from the new list means removed, as with parameters.
        assert_eq!(next.env.len(), 1);
        assert_eq!(next.env[0].value.as_deref(), Some("9"));
        assert_eq!(next.init.config_map(), Some("init-sql"));

        // A reserved name is refused on edit exactly as on create.
        assert!(matches!(
            s.apply_change(&SpecChange {
                env: Some(
                    serde_json::from_value(
                        serde_json::json!([{ "name": "PGLORD_AGENT_ROLE", "value": "primary" }])
                    )
                    .expect("env")
                ),
                ..Default::default()
            }),
            Err(SpecError::InvalidEnv(_))
        ));
    }

    #[test]
    fn credentials_cannot_be_repointed_on_a_running_cluster() {
        // Not in SpecChange at all: changing the Secret would change the
        // password the agent presents without changing pg_authid, locking out
        // every client that reads the new Secret.
        let mut s = spec("demo");
        s.credentials_secret = Some("mine".into());
        let next = s
            .apply_change(&SpecChange {
                replicas: Some(2),
                ..Default::default()
            })
            .expect("change");
        assert_eq!(next.credentials_secret(), Some("mine"));
    }

    #[test]
    fn editing_a_cluster_never_loses_where_its_data_came_from() {
        // `restore` is absent from SpecChange on purpose: it is a bootstrap
        // source, not a setting. Editing the cluster must keep it, both because
        // re-running it would be catastrophic and because it is the only record
        // that this cluster's contents are a copy of something else.
        let mut s = spec("demo");
        s.restore = Some(
            serde_json::from_value(serde_json::json!({
                "source_cluster": "src",
                "repository": {
                    "enabled": true,
                    "s3_endpoint": "https://s3.example.com",
                    "s3_bucket": "b",
                    "s3_credentials_secret": "creds"
                }
            }))
            .expect("restore"),
        );
        let next = s
            .apply_change(&SpecChange {
                replicas: Some(3),
                ..Default::default()
            })
            .expect("change");
        assert_eq!(next.restore, s.restore);
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

    #[test]
    fn pg_hba_rules_are_validated() {
        let mut s = spec("demo");
        s.pg_hba = serde_json::from_value(serde_json::json!([
            { "conn": "host", "database": "app", "user": "app_rw",
              "address": "10.0.0.0/8", "method": "scram-sha-256" }
        ]))
        .expect("rules");
        assert!(s.validate().is_ok());

        s.pg_hba = serde_json::from_value(serde_json::json!([
            { "conn": "host", "database": "all", "user": "all",
              "address": "example.com", "method": "reject" }
        ]))
        .expect("rules");
        assert!(matches!(s.validate(), Err(SpecError::InvalidHba(_))));
    }

    #[test]
    fn change_replaces_pg_hba_wholesale() {
        let s = spec("demo");
        let rules: Vec<crate::hba::HbaRule> = serde_json::from_value(serde_json::json!([
            { "conn": "host", "database": "all", "user": "intruder",
              "address": "0.0.0.0/0", "method": "reject" }
        ]))
        .expect("rules");
        let next = s
            .apply_change(&SpecChange {
                pg_hba: Some(rules.clone()),
                ..Default::default()
            })
            .expect("apply");
        assert_eq!(next.pg_hba, rules);

        // Absent leaves rules untouched; empty list clears them.
        let untouched = next
            .apply_change(&SpecChange {
                replicas: Some(2),
                ..Default::default()
            })
            .expect("apply");
        assert_eq!(untouched.pg_hba, rules);
        let cleared = next
            .apply_change(&SpecChange {
                pg_hba: Some(vec![]),
                ..Default::default()
            })
            .expect("apply");
        assert!(cleared.pg_hba.is_empty());
    }

    #[test]
    fn sync_requires_at_least_two_replicas() {
        let mut s = spec("demo");
        s.synchronous = true;
        assert_eq!(s.validate(), Err(SpecError::SyncRequiresReplicas));
        s.replicas = 2;
        assert_eq!(s.validate(), Ok(()));

        // Scaling a sync cluster down to a single instance would block every
        // write forever; the same rule catches it on the change path.
        assert_eq!(
            s.apply_change(&SpecChange {
                replicas: Some(1),
                ..Default::default()
            }),
            Err(SpecError::SyncRequiresReplicas)
        );
    }

    #[test]
    fn sync_rejects_parameters_that_defeat_it() {
        let mut s = spec("demo");
        s.replicas = 2;
        s.synchronous = true;
        for bad in ["off", "local"] {
            s.parameters = BTreeMap::from([("synchronous_commit".into(), bad.into())]);
            assert!(
                matches!(
                    s.validate(),
                    Err(SpecError::SyncConflictsSynchronousCommit(_))
                ),
                "expected rejection for synchronous_commit={bad}"
            );
        }
        // Stronger-or-equal levels stay legal tuning.
        for ok in ["on", "remote_write", "remote_apply"] {
            s.parameters = BTreeMap::from([("synchronous_commit".into(), ok.into())]);
            assert_eq!(s.validate(), Ok(()), "expected ok for synchronous_commit={ok}");
        }
        // With the flag off, 'off' remains an allowed durability trade.
        s.synchronous = false;
        s.parameters = BTreeMap::from([("synchronous_commit".into(), "off".into())]);
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn synchronous_standby_names_is_reserved() {
        let mut s = spec("demo");
        s.parameters =
            BTreeMap::from([("synchronous_standby_names".into(), "ANY 1 (x)".into())]);
        assert!(matches!(s.validate(), Err(SpecError::ReservedParameter(_))));
    }

    #[test]
    fn sync_flag_is_a_day2_toggle() {
        let mut s = spec("demo");
        s.replicas = 2;
        let on = s
            .apply_change(&SpecChange {
                synchronous: Some(true),
                ..Default::default()
            })
            .expect("enable");
        assert!(on.synchronous);
        // Absent leaves it untouched.
        let scaled = on
            .apply_change(&SpecChange {
                replicas: Some(3),
                ..Default::default()
            })
            .expect("scale");
        assert!(scaled.synchronous);
        let off = on
            .apply_change(&SpecChange {
                synchronous: Some(false),
                ..Default::default()
            })
            .expect("disable");
        assert!(!off.synchronous);
    }

    #[test]
    fn specs_without_the_sync_flag_parse_as_async() {
        // Every spec ConfigMap written before this feature lacks the field.
        assert!(!spec("demo").synchronous);
    }

    #[test]
    fn specs_without_pg_hba_deserialize_with_no_rules() {
        // Every spec ConfigMap written before this feature lacks the field.
        let s = spec("demo");
        assert!(s.pg_hba.is_empty());
    }
}
