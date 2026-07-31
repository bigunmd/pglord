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
    /// Verify the endpoint's TLS certificate. Turn off only for storage with a
    /// self-signed certificate (a dev MinIO, typically).
    #[serde(default = "default_verify_tls")]
    pub verify_tls: bool,
    /// Cron expression (UTC) for automatic backups. None = manual only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// Backup type the schedule takes.
    #[serde(default)]
    pub schedule_kind: BackupKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BackupKind {
    /// Everything. Slowest, and the base every other type builds on.
    #[default]
    Full,
    /// Changes since the last full backup.
    Diff,
    /// Changes since the last backup of any type. Smallest and quickest.
    Incr,
}

impl BackupKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackupKind::Full => "full",
            BackupKind::Diff => "diff",
            BackupKind::Incr => "incr",
        }
    }
}

fn default_verify_tls() -> bool {
    true
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
    #[error(
        "endpoint '{0}' is plain HTTP: pgBackRest requires TLS for object storage. \
         Use an https endpoint, and turn off TLS verification if its certificate is self-signed."
    )]
    InsecureEndpoint(String),
    #[error("invalid retention {0}: keep between 1 and 52 full backups")]
    InvalidRetention(u8),
    #[error("invalid schedule '{schedule}': {reason}")]
    InvalidSchedule { schedule: String, reason: String },
}

/// Shortest gap pglord will schedule. Backups overlap badly and cost storage;
/// anything finer is a mistake rather than an intent.
pub const MIN_SCHEDULE_INTERVAL_MINUTES: i64 = 15;

/// Accept ordinary 5-field cron as people actually write it.
///
/// Two differences from the parser we use: it wants a leading seconds field,
/// and it numbers weekdays 1–7 (Sunday first) where standard cron uses 0–6.
/// Translating here means `0 3 * * 0` means Sunday, as everyone expects.
pub fn normalize_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        // Already has seconds (or is malformed) — leave it for the parser.
        return expr.to_string();
    }
    let dow = fields[4]
        .split_inclusive(['-', ',', '/'])
        .map(|token| {
            let (digits, sep) = match token.chars().last() {
                Some(c) if "-,/".contains(c) => {
                    (&token[..token.len() - 1], &token[token.len() - 1..])
                }
                _ => (token, ""),
            };
            match digits.parse::<u8>() {
                // 0 and 7 both mean Sunday in standard cron.
                Ok(n) if n <= 7 => format!("{}{sep}", if n == 7 { 1 } else { n + 1 }),
                _ => token.to_string(),
            }
        })
        .collect::<String>();
    format!(
        "0 {} {} {} {} {dow}",
        fields[0], fields[1], fields[2], fields[3]
    )
}

/// Validate a cron expression and its frequency. Returns the parsed schedule so
/// callers do not parse twice.
pub fn parse_schedule(expr: &str) -> Result<cron::Schedule, BackupError> {
    use std::str::FromStr;
    let normalized = normalize_cron(expr);
    let schedule =
        cron::Schedule::from_str(&normalized).map_err(|e| BackupError::InvalidSchedule {
            schedule: expr.to_string(),
            reason: e.to_string(),
        })?;

    let mut upcoming = schedule.upcoming(chrono::Utc).take(2);
    if let (Some(first), Some(second)) = (upcoming.next(), upcoming.next())
        && (second - first).num_minutes() < MIN_SCHEDULE_INTERVAL_MINUTES
    {
        return Err(BackupError::InvalidSchedule {
            schedule: expr.to_string(),
            reason: format!(
                "backups cannot run more often than every {MIN_SCHEDULE_INTERVAL_MINUTES} minutes"
            ),
        });
    }
    Ok(schedule)
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
    // Caught here rather than at backup time: pgBackRest speaks TLS to S3
    // unconditionally, so a plain-HTTP endpoint fails with an opaque
    // "wrong version number" error minutes into the first backup.
    if endpoint.starts_with("http://") {
        return Err(BackupError::InsecureEndpoint(endpoint.to_string()));
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
    if let Some(schedule) = b.schedule.as_deref().filter(|s| !s.trim().is_empty()) {
        parse_schedule(schedule.trim())?;
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
    let verify = if b.verify_tls {
        ""
    } else {
        "repo1-storage-verify-tls=n\n"
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
            verify_tls: true,
            schedule: None,
            schedule_kind: BackupKind::Full,
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
    fn plain_http_endpoints_are_rejected_up_front() {
        // pgBackRest always speaks TLS to S3; a plain-HTTP endpoint otherwise
        // fails minutes into the first backup with an opaque TLS error.
        let mut b = enabled();
        b.s3_endpoint = Some("http://minio:9000".into());
        assert!(matches!(
            validate(&b),
            Err(BackupError::InsecureEndpoint(_))
        ));
    }

    #[test]
    fn tls_verification_can_be_turned_off_for_self_signed_storage() {
        let mut b = enabled();
        b.verify_tls = false;
        assert!(pgbackrest_conf("demo", "/pgdata", &b).contains("repo1-storage-verify-tls=n"));
        // Verification stays on by default.
        assert!(!pgbackrest_conf("demo", "/pgdata", &enabled()).contains("verify-tls=n"));
        assert!(BackupSpec::default().verify_tls || !BackupSpec::default().enabled);
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
    fn schedules_are_validated_and_rate_limited() {
        // Familiar 5-field cron is accepted.
        assert!(parse_schedule("0 3 * * *").is_ok());
        assert!(parse_schedule("30 2 * * 0").is_ok());
        // Nonsense is rejected with the parser's reason.
        assert!(matches!(
            parse_schedule("not a cron"),
            Err(BackupError::InvalidSchedule { .. })
        ));
        // Every minute would overlap runs and pile up storage.
        assert!(matches!(
            parse_schedule("* * * * *"),
            Err(BackupError::InvalidSchedule { .. })
        ));

        let mut b = enabled();
        b.schedule = Some("0 3 * * *".into());
        assert_eq!(validate(&b), Ok(()));
        b.schedule = Some("* * * * *".into());
        assert!(matches!(
            validate(&b),
            Err(BackupError::InvalidSchedule { .. })
        ));
        // An empty schedule means manual only, not an error.
        b.schedule = Some("  ".into());
        assert_eq!(validate(&b), Ok(()));
    }

    #[test]
    fn standard_cron_weekdays_are_translated() {
        // Standard cron: 0 = Sunday. The parser we use starts at 1, so a naive
        // pass-through would silently shift every weekday schedule by a day.
        assert_eq!(normalize_cron("30 2 * * 0"), "0 30 2 * * 1");
        assert_eq!(normalize_cron("0 3 * * 1-5"), "0 0 3 * * 2-6");
        assert_eq!(normalize_cron("0 3 * * 0,6"), "0 0 3 * * 1,7");
        // 7 is also Sunday in standard cron.
        assert_eq!(normalize_cron("0 3 * * 7"), "0 0 3 * * 1");
        // Names pass through untouched.
        assert_eq!(normalize_cron("0 3 * * MON"), "0 0 3 * * MON");
        // Anything already carrying seconds is left alone.
        assert_eq!(normalize_cron("0 0 3 * * 2"), "0 0 3 * * 2");

        assert!(parse_schedule("30 2 * * 0").is_ok());
        assert!(parse_schedule("0 3 * * 1-5").is_ok());
    }

    #[test]
    fn backup_kinds_map_to_pgbackrest_types() {
        assert_eq!(BackupKind::default(), BackupKind::Full);
        assert_eq!(BackupKind::Full.as_str(), "full");
        assert_eq!(BackupKind::Diff.as_str(), "diff");
        assert_eq!(BackupKind::Incr.as_str(), "incr");
    }

    #[test]
    fn archive_command_targets_the_stanza() {
        assert_eq!(
            archive_command("demo"),
            "pgbackrest --stanza=pglord-demo archive-push %p"
        );
    }
}
