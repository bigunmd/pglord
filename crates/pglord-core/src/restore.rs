//! Restoring a backup into a *new* cluster.
//!
//! Restore is never in place. Someone restoring is usually trying to rescue
//! data; overwriting the running cluster with an older copy would destroy the
//! only thing they still have. A restore therefore always produces a new
//! cluster, and the original is left untouched.

use serde::{Deserialize, Serialize};

use crate::backup::{self, BackupSpec};

#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RestoreSpec {
    /// Cluster whose repository holds the backup. Names the pgBackRest stanza
    /// that is read — which is the *source's* stanza, not this cluster's.
    pub source_cluster: String,
    /// The repository to read, copied from the source when the restore was
    /// requested rather than referenced. The source can afterwards be
    /// reconfigured or deleted without stranding this cluster's bootstrap.
    pub repository: BackupSpec,
    /// Which backup to restore. None restores the most recent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_label: Option<String>,
    /// Recover to this instant (RFC3339, UTC) instead of the end of the WAL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_time: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RestoreError {
    #[error("restore requires a source cluster")]
    MissingSource,
    #[error("the source cluster's repository is not usable: {0}")]
    InvalidRepository(#[from] backup::BackupError),
    #[error("invalid point in time '{0}': expected an RFC3339 timestamp like 2026-08-01T12:30:00Z")]
    InvalidTargetTime(String),
}

pub fn validate(r: &RestoreSpec) -> Result<(), RestoreError> {
    if r.source_cluster.trim().is_empty() {
        return Err(RestoreError::MissingSource);
    }
    // A restore reads the repository exactly as a backup writes it, so it has
    // to satisfy the same rules — including the https-only check, which would
    // otherwise fail opaquely minutes into the restore.
    backup::validate(&r.repository)?;
    if let Some(t) = r.target_time.as_deref().filter(|t| !t.trim().is_empty()) {
        pg_target_time(t)?;
    }
    Ok(())
}

/// RFC3339 -> the timestamp form postgres parses for `recovery_target_time`.
///
/// Normalised to UTC with an explicit offset: a bare local timestamp would be
/// read in the server's timezone, which is how people restore to the wrong
/// hour and never find out until the data is wrong.
pub fn pg_target_time(rfc3339: &str) -> Result<String, RestoreError> {
    let parsed = chrono::DateTime::parse_from_rfc3339(rfc3339.trim())
        .map_err(|_| RestoreError::InvalidTargetTime(rfc3339.to_string()))?;
    Ok(parsed
        .with_timezone(&chrono::Utc)
        .format("%Y-%m-%d %H:%M:%S+00")
        .to_string())
}

/// pgbackrest.conf for the restore: the *source's* stanza and repository, but
/// this instance's PGDATA as the destination.
pub fn pgbackrest_restore_conf(r: &RestoreSpec, pgdata: &str) -> String {
    backup::pgbackrest_conf(&r.source_cluster, pgdata, &r.repository)
}

/// The pgbackrest argv for the restore, after the leading `--stanza=`.
///
/// Three shapes, and the recovery type is what separates them:
/// - nothing selected: the newest backup, then every archived WAL record after
///   it — the most recent state the repository can produce
/// - a backup: that backup's contents, and nothing after it (`--type=immediate`)
/// - a time: replay up to that instant and stop
///
/// The middle case is the one worth stating: pgBackRest's *default* recovery
/// type rolls forward to the end of the WAL even when a specific backup is
/// selected, so asking for an old backup would silently hand back the present.
pub fn restore_args(r: &RestoreSpec) -> Vec<String> {
    // No --delta: the agent clears PGDATA first, so there is never anything to
    // reconcile against, and a plain restore is the easier thing to reason about.
    let mut args = vec!["restore".to_string()];
    let label = r.backup_label.as_deref().filter(|l| !l.trim().is_empty());
    let target = r
        .target_time
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .and_then(|t| pg_target_time(t).ok());

    if let Some(label) = label {
        args.push(format!("--set={}", label.trim()));
    }
    match (&target, label) {
        (Some(target), _) => {
            args.push("--type=time".to_string());
            args.push(format!("--target={target}"));
        }
        (None, Some(_)) => args.push("--type=immediate".to_string()),
        (None, None) => {}
    }
    // Any recovery target at all needs this. PostgreSQL's default
    // `recovery_target_action` is *pause*: on reaching the target it stops and
    // waits for a human, leaving an instance that passes its readiness probe,
    // serves reads, and never accepts a write. Recovery to the end of the WAL
    // sets no target and ends on its own, so it neither needs nor accepts it.
    if args.iter().any(|a| a.starts_with("--type=")) {
        args.push("--target-action=promote".to_string());
    }
    args
}

/// The stanza a restore reads.
pub fn source_stanza(r: &RestoreSpec) -> String {
    backup::stanza_name(&r.source_cluster)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> BackupSpec {
        // Through serde, so the serde defaults (retention, region, verify_tls)
        // apply — `BackupSpec::default()` is the *Default derive*, which leaves
        // retention at 0 and would not validate.
        serde_json::from_value(serde_json::json!({
            "enabled": true,
            "s3_endpoint": "https://s3.example.com",
            "s3_bucket": "pg-backups",
            "s3_credentials_secret": "backup-creds"
        }))
        .expect("repository")
    }

    fn spec() -> RestoreSpec {
        RestoreSpec {
            source_cluster: "orders".into(),
            repository: repo(),
            backup_label: None,
            target_time: None,
        }
    }

    #[test]
    fn latest_backup_needs_no_selection_flags() {
        let args = restore_args(&spec());
        assert_eq!(args[0], "restore");
        assert!(!args.iter().any(|a| a.starts_with("--set")));
        assert!(!args.iter().any(|a| a.starts_with("--type")));
    }

    #[test]
    fn a_named_backup_stops_at_that_backup() {
        let mut r = spec();
        r.backup_label = Some("20260731-120000F".into());
        let args = restore_args(&r);
        assert!(args.contains(&"--set=20260731-120000F".to_string()));
        // The point of the whole test: pgBackRest's default recovery type
        // replays every archived WAL record after the selected backup, so
        // asking for last week's backup would quietly return today's data.
        assert!(args.contains(&"--type=immediate".to_string()));
        // immediate is a recovery *target*, so postgres would otherwise pause
        // on reaching it and never leave recovery.
        assert!(args.contains(&"--target-action=promote".to_string()));
    }

    #[test]
    fn every_recovery_target_promotes_and_no_target_does_not() {
        // The default action on reaching a target is pause: the instance would
        // pass its readiness probe, serve reads, and refuse every write forever.
        for r in [
            RestoreSpec {
                target_time: Some("2026-08-01T12:30:00Z".into()),
                ..spec()
            },
            RestoreSpec {
                backup_label: Some("20260731-120000F".into()),
                ..spec()
            },
        ] {
            assert!(
                restore_args(&r).contains(&"--target-action=promote".to_string()),
                "a recovery target must promote, got {:?}",
                restore_args(&r)
            );
        }
        // Recovery to the end of the WAL sets no target: it ends by itself, and
        // pgBackRest rejects target-action without a target type.
        let args = restore_args(&spec());
        assert!(!args.iter().any(|a| a.starts_with("--target-action")));
    }

    #[test]
    fn a_time_wins_over_the_backup_it_starts_from() {
        // Both selected: start from that backup, recover to the instant. Adding
        // --type=immediate here would stop at the backup and never reach it.
        let mut r = spec();
        r.backup_label = Some("20260731-120000F".into());
        r.target_time = Some("2026-08-01T12:30:00Z".into());
        let args = restore_args(&r);
        assert!(args.contains(&"--set=20260731-120000F".to_string()));
        assert!(args.contains(&"--type=time".to_string()));
        assert!(!args.contains(&"--type=immediate".to_string()));
    }

    #[test]
    fn point_in_time_restores_promote_rather_than_pause() {
        let mut r = spec();
        r.target_time = Some("2026-08-01T12:30:00Z".into());
        let args = restore_args(&r);
        assert!(args.contains(&"--type=time".to_string()));
        assert!(args.contains(&"--target=2026-08-01 12:30:00+00".to_string()));
        // Postgres' default is to pause at the target and wait for someone to
        // notice; the instance would look healthy while refusing every write.
        assert!(args.contains(&"--target-action=promote".to_string()));
    }

    #[test]
    fn target_times_are_normalised_to_utc() {
        // The same instant written in another offset must produce the same
        // target, or a restore lands hours away from where it was asked to.
        assert_eq!(
            pg_target_time("2026-08-01T12:30:00Z").expect("utc"),
            "2026-08-01 12:30:00+00"
        );
        assert_eq!(
            pg_target_time("2026-08-01T14:30:00+02:00").expect("offset"),
            "2026-08-01 12:30:00+00"
        );
        for bad in ["yesterday", "2026-08-01", "", "2026-13-01T00:00:00Z"] {
            assert!(
                pg_target_time(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn restore_reads_the_source_stanza_into_our_pgdata() {
        let conf = pgbackrest_restore_conf(&spec(), "/var/lib/postgresql/data/pgdata");
        // The stanza is the SOURCE's: that is where the backup lives. Using this
        // cluster's own name would look for a stanza that does not exist.
        assert!(conf.contains("[pglord-orders]"));
        assert!(conf.contains("pg1-path=/var/lib/postgresql/data/pgdata"));
        assert!(conf.contains("repo1-s3-bucket=pg-backups"));
        assert_eq!(source_stanza(&spec()), "pglord-orders");
    }

    #[test]
    fn a_restore_needs_a_source_and_a_usable_repository() {
        assert_eq!(validate(&spec()), Ok(()));

        let mut r = spec();
        r.source_cluster = "  ".into();
        assert_eq!(validate(&r), Err(RestoreError::MissingSource));

        // The repository rules are the backup rules; plain HTTP fails the same
        // way here as it does for a backup.
        let mut r = spec();
        r.repository.s3_endpoint = Some("http://minio:9000".into());
        assert!(matches!(
            validate(&r),
            Err(RestoreError::InvalidRepository(_))
        ));

        let mut r = spec();
        r.repository.s3_bucket = None;
        assert!(matches!(
            validate(&r),
            Err(RestoreError::InvalidRepository(_))
        ));

        let mut r = spec();
        r.target_time = Some("last tuesday".into());
        assert!(matches!(
            validate(&r),
            Err(RestoreError::InvalidTargetTime(_))
        ));
    }
}
