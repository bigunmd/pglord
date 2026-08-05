//! Blue-green major upgrade: the durable state stored on the spec ConfigMap
//! and the pure naming/SQL pieces of the logical-replication migration.
//!
//! A major upgrade never touches an existing data directory — pg_upgrade is
//! deliberately not used. Instead a second ("green") instance set on the
//! target image is built next to the running one and kept in sync by logical
//! replication until the user cuts over.

use serde::{Deserialize, Serialize};

/// Annotation on the spec ConfigMap holding the serialized [`UpgradeState`].
/// CAS-updated like the primary record; absent = no upgrade in flight.
pub const UPGRADE_ANNOTATION: &str = "pglord.dev/upgrade";

/// One publication of this name per user database on the source side.
pub const PUBLICATION: &str = "pglord_upgrade";

/// How a major upgrade is performed. Absent from state written before
/// in-place existed, which must keep meaning blue-green.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UpgradeMode {
    /// Build a second instance set and replicate into it. No downtime, at the
    /// cost of a full second copy of the cluster.
    #[default]
    BlueGreen,
    /// Convert this cluster's own data directory with `pg_upgrade --link`.
    /// Minutes of downtime and almost no extra disk, recoverable only from a
    /// backup.
    InPlace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UpgradePhase {
    /// The green set exists and is (or is becoming) a logical replica of the
    /// running cluster. Applications are untouched; aborting is free.
    Migrating,
    /// Cutover happened: applications write to green, the old set is fenced
    /// read-only and kept as the rollback target.
    Soaking,
    /// In-place: the instances are converting their data directory. This is
    /// the downtime window.
    Upgrading,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
pub struct UpgradeState {
    pub target_image: String,
    /// The generation the green StatefulSet renders as (current + 1).
    pub next_generation: String,
    pub phase: UpgradePhase,
    /// The recorded primary before cutover; rollback restores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_primary: Option<String>,
    #[serde(default)]
    pub mode: UpgradeMode,
    /// The major being upgraded from, resolved once at start so the render
    /// stays stable across reconciles. In-place only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_major: Option<u32>,
}

/// The PostgreSQL major an image tag advertises, when it says so.
///
/// Tags are arbitrary strings, so this is a convenience and never an
/// authority — the running server's own `server_version_num` decides.
/// Recognises a bare major ("…:17"), a leading major with a suffix
/// ("…:18-timescaledb") and a trailing "-pgNN" ("…:v1.2.3-pg16").
pub fn major_from_image(image: &str) -> Option<u32> {
    // Strip a registry host:port before looking for the tag separator: the
    // colon in "registry:5000/img" does not introduce a tag.
    let (host, tag) = image.rsplit_once(':')?;
    if host.is_empty() || tag.contains('/') {
        return None;
    }
    if let Some((_, pg)) = tag.rsplit_once("-pg") {
        return pg.parse().ok();
    }
    let head: String = tag.chars().take_while(char::is_ascii_digit).collect();
    head.parse().ok()
}

/// The generation an upgrade started from `current` produces. Unparseable
/// input restarts the count rather than panicking: the value only has to be
/// distinct from the current one to give the green set its own object names.
pub fn next_generation(current: &str) -> String {
    let n: u32 = current
        .strip_prefix('g')
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    format!("g{}", n + 1)
}

/// Subscription (and therefore replication-slot) name for a database. Slot
/// names accept only lowercase alphanumerics and underscores while database
/// names accept far more, so everything else maps to '_'.
pub fn subscription_name(db: &str) -> String {
    let safe: String = db
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("pglord_upgrade_{safe}")
}

pub fn create_publication_sql() -> String {
    format!("CREATE PUBLICATION {PUBLICATION} FOR ALL TABLES")
}

pub fn drop_publication_sql() -> String {
    format!("DROP PUBLICATION IF EXISTS {PUBLICATION}")
}

/// `conninfo` is embedded as a SQL string literal, so single quotes double.
pub fn create_subscription_sql(db: &str, conninfo: &str) -> String {
    format!(
        "CREATE SUBSCRIPTION \"{}\" CONNECTION '{}' PUBLICATION {PUBLICATION}",
        subscription_name(db),
        conninfo.replace('\'', "''"),
    )
}

pub fn drop_subscription_sql(db: &str) -> String {
    format!("DROP SUBSCRIPTION IF EXISTS \"{}\"", subscription_name(db))
}

/// Fraction of an instance's volume the migration's replication slot may pin.
const WAL_RETENTION_FRACTION: u64 = 4; // a quarter

/// The `max_slot_wal_keep_size` to hold the source to while a migration runs.
///
/// A logical slot retains every WAL segment from its creation until the
/// subscriber confirms it, and the initial copy of a large database runs for
/// hours. Unbounded, that fills the *running* cluster's volume and takes down
/// the database the upgrade was supposed to leave untouched. Capping it means
/// a migration that falls too far behind loses its slot and fails visibly —
/// the upgrade is abandoned instead of the production cluster.
///
/// None when the size cannot be parsed: better to leave postgres's own
/// default than to derive a limit from a value we do not understand.
pub fn wal_retention_for(storage_size: &str) -> Option<String> {
    let bytes = crate::spec::quantity_bytes(storage_size)?;
    let mb = (bytes / WAL_RETENTION_FRACTION) / (1 << 20);
    // Below postgres's own minimum useful window the cap would invalidate the
    // slot almost immediately, which is worse than not capping at all.
    (mb >= 64).then(|| format!("{mb}MB"))
}

/// A postgres size literal: digits with an optional unit, nothing else.
///
/// The value reaches the agent over its control endpoint, which is
/// unauthenticated by design, and lands inside a single-quoted `ALTER SYSTEM`
/// string executed as superuser. Without this check a crafted value closes the
/// quote and appends arbitrary statements — the same reason role passwords are
/// charset-checked before they are dollar-quoted.
pub fn is_valid_wal_size(s: &str) -> bool {
    let digits = s.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &s[digits.len()..];
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && matches!(unit, "" | "kB" | "MB" | "GB" | "TB")
}

/// None when `size` is not a plain size literal; the caller must reject it
/// rather than interpolate it.
pub fn set_wal_retention_sql(size: &str) -> Option<String> {
    is_valid_wal_size(size).then(|| format!("ALTER SYSTEM SET max_slot_wal_keep_size = '{size}'"))
}

pub const RESET_WAL_RETENTION_SQL: &str = "ALTER SYSTEM RESET max_slot_wal_keep_size";

/// Slots whose retained WAL was removed: the migration behind them is dead and
/// cannot be resumed, only restarted.
pub const LOST_SLOTS_SQL: &str = "SELECT count(*)::int8 FROM pg_replication_slots \
     WHERE slot_name LIKE 'pglord\\_upgrade\\_%' AND wal_status = 'lost'";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_generation_increments() {
        assert_eq!(next_generation("g1"), "g2");
        assert_eq!(next_generation("g9"), "g10");
        // Garbage never panics; it restarts the count.
        assert_eq!(next_generation("weird"), "g2");
    }

    #[test]
    fn subscription_names_are_slot_safe() {
        assert_eq!(subscription_name("appdb"), "pglord_upgrade_appdb");
        assert_eq!(subscription_name("My-DB.1"), "pglord_upgrade_my_db_1");
    }

    #[test]
    fn subscription_conninfo_quotes_are_escaped() {
        let sql = create_subscription_sql("appdb", "host=x password=it's");
        assert!(sql.contains("password=it''s"), "{sql}");
        assert!(sql.contains("PUBLICATION pglord_upgrade"), "{sql}");
        assert!(sql.contains("\"pglord_upgrade_appdb\""), "{sql}");
    }

    #[test]
    fn wal_retention_is_a_quarter_of_the_volume() {
        assert_eq!(wal_retention_for("1Ti").as_deref(), Some("262144MB"));
        assert_eq!(wal_retention_for("4Gi").as_deref(), Some("1024MB"));
        assert_eq!(wal_retention_for("1Gi").as_deref(), Some("256MB"));
        // Too small to be a useful window: leave postgres's default alone
        // rather than invalidate the slot the moment it is created.
        assert_eq!(wal_retention_for("100Mi"), None);
        // Unparseable: never derive a limit from a value we do not understand.
        assert_eq!(wal_retention_for("banana"), None);
    }

    #[test]
    fn wal_size_rejects_anything_but_a_size_literal() {
        for good in ["256MB", "1GB", "1024", "4TB", "512kB"] {
            assert!(is_valid_wal_size(good), "{good} should be accepted");
            assert!(set_wal_retention_sql(good).is_some());
        }
        // The endpoint is unauthenticated and the value is single-quoted into
        // an ALTER SYSTEM run as superuser: everything else must be refused.
        for bad in [
            "",
            "MB",
            "1MB'; DROP DATABASE postgres; --",
            "1 MB",
            "-1MB",
            "1mb",
            "1MB2",
            "$(whoami)",
            "1MB\nALTER SYSTEM SET fsync=off",
        ] {
            assert!(!is_valid_wal_size(bad), "{bad:?} should be rejected");
            assert_eq!(set_wal_retention_sql(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn state_roundtrips_through_json() {
        let st = UpgradeState {
            target_image: "pglord-agent:18".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Migrating,
            previous_primary: None,
            mode: UpgradeMode::BlueGreen,
            from_major: None,
        };
        let json = serde_json::to_string(&st).expect("serialize");
        assert!(json.contains("\"migrating\""), "{json}");
        let back: UpgradeState = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, st);
    }

    #[test]
    fn mode_defaults_to_blue_green_for_stored_state() {
        // State written before in-place existed carries no mode; it must keep
        // parsing, as blue-green, or a deploy breaks every in-flight upgrade.
        let st: UpgradeState = serde_json::from_str(
            r#"{"target_image":"a:18","next_generation":"g2","phase":"migrating"}"#,
        )
        .expect("parse");
        assert_eq!(st.mode, UpgradeMode::BlueGreen);
        assert_eq!(st.from_major, None);
    }

    #[test]
    fn in_place_state_roundtrips() {
        let st = UpgradeState {
            target_image: "pglord-agent:17".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Upgrading,
            previous_primary: None,
            mode: UpgradeMode::InPlace,
            from_major: Some(16),
        };
        let json = serde_json::to_string(&st).expect("serialize");
        assert!(json.contains("\"in_place\""), "{json}");
        assert!(json.contains("\"upgrading\""), "{json}");
        assert_eq!(
            serde_json::from_str::<UpgradeState>(&json).expect("parse"),
            st
        );
    }

    #[test]
    fn major_parses_from_conventional_image_tags() {
        assert_eq!(major_from_image("pglord-agent:17"), Some(17));
        assert_eq!(
            major_from_image("ghcr.io/o/pglord-agent:18-timescaledb"),
            Some(18)
        );
        assert_eq!(
            major_from_image("ghcr.io/o/pglord-agent:v1.2.3-pg16"),
            Some(16)
        );
        // Tags are arbitrary: an unreadable one is not an error, it just means
        // the running server's reported version is the only authority.
        assert_eq!(major_from_image("pglord-agent:dev"), None);
        assert_eq!(major_from_image("pglord-agent"), None);
        assert_eq!(major_from_image("registry:5000/pglord-agent"), None);
    }
}
