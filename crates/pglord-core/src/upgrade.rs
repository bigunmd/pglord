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

pub fn set_wal_retention_sql(size: &str) -> String {
    format!("ALTER SYSTEM SET max_slot_wal_keep_size = '{size}'")
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
    fn state_roundtrips_through_json() {
        let st = UpgradeState {
            target_image: "pglord-agent:18".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Migrating,
            previous_primary: None,
        };
        let json = serde_json::to_string(&st).expect("serialize");
        assert!(json.contains("\"migrating\""), "{json}");
        let back: UpgradeState = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, st);
    }
}
