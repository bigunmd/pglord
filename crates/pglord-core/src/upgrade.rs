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
