//! Backup repository configuration (pgBackRest to S3-compatible storage).
//!
//! The spec holds only the *name* of a Secret with the object-storage
//! credentials — keys never enter the cluster spec, which is a plain ConfigMap
//! readable by anything that can read the namespace.

use serde::{Deserialize, Serialize};

pub const STANZA_PREFIX: &str = "pglord";
pub const CONF_PATH: &str = "/etc/pgbackrest/pgbackrest.conf";
pub const ACCESS_KEY_KEY: &str = "access-key";
pub const SECRET_KEY_KEY: &str = "secret-key";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BackupSpec {
    /// Backups (and therefore WAL archiving) are off unless this is set.
    #[serde(default)]
    pub enabled: bool,
    /// S3-compatible endpoint, e.g. "https://s3.amazonaws.com" or a MinIO URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_bucket: Option<String>,
    #[serde(default = "default_region")]
    pub s3_region: String,
    /// Secret holding `access-key` and `secret-key`. Values never live here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_credentials_secret: Option<String>,
    /// Full backups to keep. pgBackRest expires older ones automatically.
    #[serde(default = "default_retention")]
    pub retention_full: u8,
    /// Optional prefix inside the bucket, so several clusters can share one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
}

fn default_region() -> String {
    "us-east-1".into()
}
fn default_retention() -> u8 {
    2
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BackupError {
    #[error("backups require {0}")]
    Missing(&'static str),
    #[error("invalid endpoint '{0}': expected a URL like https://s3.example.com")]
    InvalidEndpoint(String),
    #[error("invalid retention {0}: keep between 1 and 52 full backups")]
    InvalidRetention(u8),
}

pub fn stanza_name(cluster: &str) -> String {
    format!("{STANZA_PREFIX}-{cluster}")
}

pub fn validate(b: &BackupSpec) -> Result<(), BackupError> {
    if !b.enabled {
        return Ok(());
    }
    let endpoint = b
        .s3_endpoint
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or(BackupError::Missing("an S3 endpoint"))?;
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err(BackupError::InvalidEndpoint(endpoint.to_string()));
    }
    if b.s3_bucket.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return Err(BackupError::Missing("a bucket"));
    }
    if b.s3_credentials_secret
        .as_deref()
        .is_none_or(|s| s.trim().is_empty())
    {
        return Err(BackupError::Missing("a credentials secret"));
    }
    if b.retention_full < 1 || b.retention_full > 52 {
        return Err(BackupError::InvalidRetention(b.retention_full));
    }
    Ok(())
}

/// Human-readable repository location for the UI.
pub fn repository_uri(b: &BackupSpec) -> Option<String> {
    let bucket = b.s3_bucket.as_deref()?;
    Some(match b.path_prefix.as_deref().filter(|p| !p.is_empty()) {
        Some(prefix) => format!("s3://{bucket}/{}", prefix.trim_matches('/')),
        None => format!("s3://{bucket}"),
    })
}

/// pgbackrest.conf body. Credentials are supplied through the environment
/// (PGBACKREST_REPO1_S3_KEY / _KEY_SECRET), never written to this file.
pub fn pgbackrest_conf(cluster: &str, pgdata: &str, b: &BackupSpec) -> String {
    let stanza = stanza_name(cluster);
    let endpoint = b.s3_endpoint.as_deref().unwrap_or_default();
    let host = endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let verify = if endpoint.starts_with("http://") {
        // Plain-HTTP endpoints (a dev MinIO) have no certificate to verify.
        "repo1-storage-verify-tls=n\n"
    } else {
        ""
    };
    let path = match b.path_prefix.as_deref().filter(|p| !p.is_empty()) {
        Some(prefix) => format!("/{}/{stanza}", prefix.trim_matches('/')),
        None => format!("/{stanza}"),
    };
    format!(
        "[global]\n\
         repo1-type=s3\n\
         repo1-s3-endpoint={host}\n\
         repo1-s3-bucket={}\n\
         repo1-s3-region={}\n\
         repo1-s3-uri-style=path\n\
         repo1-path={path}\n\
         repo1-retention-full={}\n\
         {verify}\
         start-fast=y\n\
         log-level-console=info\n\
         log-level-file=off\n\
         \n\
         [{stanza}]\n\
         pg1-path={pgdata}\n",
        b.s3_bucket.as_deref().unwrap_or_default(),
        b.s3_region,
        b.retention_full,
    )
}

/// The archive_command postgres uses when backups are on.
pub fn archive_command(cluster: &str) -> String {
    format!(
        "pgbackrest --stanza={} archive-push %p",
        stanza_name(cluster)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> BackupSpec {
        BackupSpec {
            enabled: true,
            s3_endpoint: Some("https://s3.example.com".into()),
            s3_bucket: Some("pg-backups".into()),
            s3_region: "eu-west-1".into(),
            s3_credentials_secret: Some("backup-creds".into()),
            retention_full: 3,
            path_prefix: None,
        }
    }

    #[test]
    fn disabled_backups_need_no_configuration() {
        assert_eq!(validate(&BackupSpec::default()), Ok(()));
    }

    #[test]
    fn enabling_requires_endpoint_bucket_and_secret() {
        let mut b = enabled();
        b.s3_endpoint = None;
        assert!(matches!(validate(&b), Err(BackupError::Missing(_))));

        let mut b = enabled();
        b.s3_bucket = Some("  ".into());
        assert!(matches!(validate(&b), Err(BackupError::Missing(_))));

        let mut b = enabled();
        b.s3_credentials_secret = None;
        assert!(matches!(validate(&b), Err(BackupError::Missing(_))));

        let mut b = enabled();
        b.s3_endpoint = Some("s3.example.com".into());
        assert!(matches!(validate(&b), Err(BackupError::InvalidEndpoint(_))));

        let mut b = enabled();
        b.retention_full = 0;
        assert!(matches!(
            validate(&b),
            Err(BackupError::InvalidRetention(0))
        ));

        assert_eq!(validate(&enabled()), Ok(()));
    }

    #[test]
    fn conf_carries_repository_settings_but_never_credentials() {
        let conf = pgbackrest_conf("demo", "/var/lib/postgresql/data/pgdata", &enabled());
        assert!(conf.contains("repo1-type=s3"));
        assert!(conf.contains("repo1-s3-endpoint=s3.example.com"));
        assert!(conf.contains("repo1-s3-bucket=pg-backups"));
        assert!(conf.contains("repo1-s3-region=eu-west-1"));
        assert!(conf.contains("repo1-retention-full=3"));
        assert!(conf.contains("[pglord-demo]"));
        assert!(conf.contains("pg1-path=/var/lib/postgresql/data/pgdata"));
        // Keys arrive through the environment; the file must stay clean.
        assert!(!conf.contains("s3-key"));
        assert!(!conf.contains("secret"));
    }

    #[test]
    fn plain_http_endpoints_skip_tls_verification() {
        let mut b = enabled();
        b.s3_endpoint = Some("http://minio:9000".into());
        let conf = pgbackrest_conf("demo", "/pgdata", &b);
        assert!(conf.contains("repo1-storage-verify-tls=n"));
        // An https endpoint must keep verification on.
        assert!(!pgbackrest_conf("demo", "/pgdata", &enabled()).contains("verify-tls=n"));
    }

    #[test]
    fn path_prefix_scopes_the_repository() {
        let mut b = enabled();
        b.path_prefix = Some("/team-a/".into());
        assert!(pgbackrest_conf("demo", "/pgdata", &b).contains("repo1-path=/team-a/pglord-demo"));
        assert_eq!(
            repository_uri(&b).as_deref(),
            Some("s3://pg-backups/team-a")
        );
        assert_eq!(
            repository_uri(&enabled()).as_deref(),
            Some("s3://pg-backups")
        );
    }

    #[test]
    fn archive_command_targets_the_stanza() {
        assert_eq!(
            archive_command("demo"),
            "pgbackrest --stanza=pglord-demo archive-push %p"
        );
    }
}
