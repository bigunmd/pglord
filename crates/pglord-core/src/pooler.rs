//! Managed PgBouncer in front of the primary.
//!
//! A separate Deployment (`<name>-pooler`) behind a `<name>-pool` Service,
//! pointed at the `-rw` Service so failover handling stays where it already
//! is. Clients authenticate with their own credentials via `auth_query`; the
//! only credential pglord materialises for the pooler is its dedicated
//! `pglord_pooler` role. Knobs are curated: everything else in pgbouncer.ini
//! is pglord-managed, same stance as the GUC catalog.

use serde::{Deserialize, Serialize};

/// The role PgBouncer connects and runs `auth_query` as.
pub const POOLER_ROLE: &str = "pglord_pooler";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PoolMode {
    /// Server connections are returned to the pool at transaction end. The
    /// right default for connection-hungry apps; session state (prepared
    /// statements, advisory locks) does not survive across transactions.
    #[default]
    Transaction,
    /// One server connection per client for the whole session.
    Session,
}

impl PoolMode {
    pub fn as_ini(&self) -> &'static str {
        match self {
            PoolMode::Transaction => "transaction",
            PoolMode::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PoolerSpec {
    /// Run a PgBouncer Deployment for this cluster. Disabling deletes the
    /// pooler objects; postgres instances are never touched by pooler changes.
    #[serde(default)]
    pub enabled: bool,
    /// Pooler pods — independent of postgres replicas.
    #[serde(default = "default_replicas")]
    pub replicas: u8,
    #[serde(default)]
    pub pool_mode: PoolMode,
    /// Client connections PgBouncer accepts, across all databases.
    #[serde(default = "default_max_client_conn")]
    pub max_client_conn: u32,
    /// Server connections per database/user pair.
    #[serde(default = "default_pool_size")]
    pub default_pool_size: u32,
    /// PgBouncer image; pinned default, overridable like the cluster image.
    #[serde(default = "default_image")]
    pub image: String,
    /// Resources of the pooler container.
    #[serde(default)]
    pub resources: crate::workload::Resources,
}

impl Default for PoolerSpec {
    fn default() -> Self {
        Self {
            enabled: false,
            replicas: default_replicas(),
            pool_mode: PoolMode::default(),
            max_client_conn: default_max_client_conn(),
            default_pool_size: default_pool_size(),
            image: default_image(),
            resources: crate::workload::Resources::default(),
        }
    }
}

fn default_replicas() -> u8 {
    1
}
fn default_max_client_conn() -> u32 {
    100
}
fn default_pool_size() -> u32 {
    20
}
fn default_image() -> String {
    "pgbouncer/pgbouncer:1.24.1".into()
}

const MAX_POOLER_REPLICAS: u8 = 10;
const MAX_CLIENT_CONN_LIMIT: u32 = 100_000;
const MAX_POOL_SIZE_LIMIT: u32 = 10_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PoolerError {
    #[error("invalid pooler replicas {0}: must be between 1 and 10")]
    InvalidReplicas(u8),
    #[error("invalid max_client_conn {0}: must be between 1 and 100000")]
    InvalidMaxClientConn(u32),
    #[error("invalid default_pool_size {0}: must be between 1 and 10000")]
    InvalidPoolSize(u32),
    #[error("pooler image must not be empty")]
    InvalidImage,
}

/// A disabled pooler validates unconditionally — inert knobs must not block
/// an unrelated spec save (same stance as backup's schedule fields).
pub fn validate(p: &PoolerSpec) -> Result<(), PoolerError> {
    if !p.enabled {
        return Ok(());
    }
    if p.replicas < 1 || p.replicas > MAX_POOLER_REPLICAS {
        return Err(PoolerError::InvalidReplicas(p.replicas));
    }
    if p.max_client_conn < 1 || p.max_client_conn > MAX_CLIENT_CONN_LIMIT {
        return Err(PoolerError::InvalidMaxClientConn(p.max_client_conn));
    }
    if p.default_pool_size < 1 || p.default_pool_size > MAX_POOL_SIZE_LIMIT {
        return Err(PoolerError::InvalidPoolSize(p.default_pool_size));
    }
    if p.image.trim().is_empty() {
        return Err(PoolerError::InvalidImage);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_disabled_transaction_mode() {
        let p: PoolerSpec = serde_json::from_value(serde_json::json!({})).expect("pooler");
        assert!(!p.enabled);
        assert_eq!(p.replicas, 1);
        assert_eq!(p.pool_mode, PoolMode::Transaction);
        assert_eq!(p.max_client_conn, 100);
        assert_eq!(p.default_pool_size, 20);
        assert_eq!(p.image, "pgbouncer/pgbouncer:1.24.1");
    }

    #[test]
    fn bounds_are_enforced() {
        let ok: PoolerSpec =
            serde_json::from_value(serde_json::json!({ "enabled": true })).expect("p");
        assert!(validate(&ok).is_ok());
        let mut p = ok.clone();
        p.replicas = 0;
        assert!(matches!(validate(&p), Err(PoolerError::InvalidReplicas(0))));
        let mut p = ok.clone();
        p.replicas = 11;
        assert!(matches!(
            validate(&p),
            Err(PoolerError::InvalidReplicas(11))
        ));
        let mut p = ok.clone();
        p.max_client_conn = 0;
        assert!(matches!(
            validate(&p),
            Err(PoolerError::InvalidMaxClientConn(0))
        ));
        let mut p = ok.clone();
        p.default_pool_size = 0;
        assert!(matches!(validate(&p), Err(PoolerError::InvalidPoolSize(0))));
        let mut p = ok;
        p.image = "  ".into();
        assert!(matches!(validate(&p), Err(PoolerError::InvalidImage)));
    }

    #[test]
    fn disabled_pooler_with_bad_knobs_still_validates() {
        let mut p: PoolerSpec = serde_json::from_value(serde_json::json!({})).expect("p");
        p.replicas = 0;
        assert!(validate(&p).is_ok());
    }
}
