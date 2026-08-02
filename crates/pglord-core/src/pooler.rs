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
    "ghcr.io/cloudnative-pg/pgbouncer:1.25.2".into()
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

/// Port PgBouncer listens on inside the pooler pod. The `<name>-pool`
/// Service still exposes 5432, so client connection strings only change host.
pub const PGBOUNCER_PORT: i32 = 6432;

/// The managed pgbouncer.ini. Fully pglord-owned: the spec contributes the
/// curated knobs, nothing else. `auth_dbname = postgres` pins `auth_query`
/// to the one database where the agent maintains the lookup function,
/// whatever database the client targets. On TLS clusters the pooler
/// terminates TLS with the same mounted cert as the instances and requires
/// TLS upstream, so `hostssl`-only clusters keep working through the pool.
pub fn pgbouncer_ini(spec: &crate::spec::ClusterSpec) -> String {
    let p = &spec.pooler;
    let mut ini = format!(
        "[databases]\n\
         * = host={name}-rw port={pg} auth_user={role}\n\
         \n\
         [pgbouncer]\n\
         listen_addr = *\n\
         listen_port = {port}\n\
         auth_type = scram-sha-256\n\
         auth_file = /etc/pgbouncer/userlist.txt\n\
         auth_user = {role}\n\
         auth_query = SELECT usename, passwd FROM pglord.pooler_lookup($1)\n\
         auth_dbname = postgres\n\
         pool_mode = {mode}\n\
         max_client_conn = {mcc}\n\
         default_pool_size = {dps}\n\
         ignore_startup_parameters = extra_float_digits,options\n",
        name = spec.name,
        pg = crate::render::PG_PORT,
        role = POOLER_ROLE,
        port = PGBOUNCER_PORT,
        mode = p.pool_mode.as_ini(),
        mcc = p.max_client_conn,
        dps = p.default_pool_size,
    );
    if spec.tls.enabled() {
        ini.push_str(
            "client_tls_sslmode = allow\n\
             client_tls_cert_file = /etc/pglord/tls/tls.crt\n\
             client_tls_key_file = /etc/pglord/tls/tls.key\n\
             server_tls_sslmode = require\n",
        );
    }
    ini
}

/// The auth_file. Exactly one entry — the pooler's own role. App roles never
/// land in a file; they are looked up live via `auth_query`.
pub fn userlist(password: &str) -> String {
    format!("\"{POOLER_ROLE}\" \"{password}\"\n")
}

/// Content hash stamped on the pooler pod template, so any config or
/// password change rolls the pooler pods. sha256 rather than a std hasher:
/// the value must be stable across server versions and restarts, or every
/// server upgrade would churn the pooler.
pub fn config_hash(ini: &str, userlist: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(ini.as_bytes());
    hasher.update([0u8]);
    hasher.update(userlist.as_bytes());
    format!("{:x}", hasher.finalize())
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
        assert_eq!(p.image, "ghcr.io/cloudnative-pg/pgbouncer:1.25.2");
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

    #[test]
    fn ini_targets_rw_service_with_auth_query() {
        let spec: crate::spec::ClusterSpec = serde_json::from_value(
            serde_json::json!({ "name": "demo", "pooler": { "enabled": true } }),
        )
        .expect("spec");
        let ini = pgbouncer_ini(&spec);
        assert!(ini.contains("* = host=demo-rw port=5432 auth_user=pglord_pooler"));
        assert!(ini.contains("listen_port = 6432"));
        assert!(ini.contains("auth_type = scram-sha-256"));
        assert!(ini.contains("auth_query = SELECT usename, passwd FROM pglord.pooler_lookup($1)"));
        assert!(ini.contains("auth_dbname = postgres"));
        assert!(ini.contains("pool_mode = transaction"));
        assert!(ini.contains("max_client_conn = 100"));
        assert!(ini.contains("default_pool_size = 20"));
        assert!(
            !ini.contains("client_tls"),
            "no tls lines on a plain cluster"
        );
    }

    #[test]
    fn session_mode_and_knobs_reach_the_ini() {
        let spec: crate::spec::ClusterSpec = serde_json::from_value(serde_json::json!({
            "name": "demo",
            "pooler": {
                "enabled": true,
                "pool_mode": "session",
                "max_client_conn": 500,
                "default_pool_size": 7
            }
        }))
        .expect("spec");
        let ini = pgbouncer_ini(&spec);
        assert!(ini.contains("pool_mode = session"));
        assert!(ini.contains("max_client_conn = 500"));
        assert!(ini.contains("default_pool_size = 7"));
    }

    #[test]
    fn tls_cluster_gets_client_and_server_tls_lines() {
        let spec: crate::spec::ClusterSpec = serde_json::from_value(serde_json::json!({
            "name": "demo",
            "pooler": { "enabled": true },
            "tls": { "server_secret": "demo-tls" }
        }))
        .expect("spec");
        let ini = pgbouncer_ini(&spec);
        assert!(ini.contains("client_tls_sslmode = allow"));
        assert!(ini.contains("client_tls_cert_file = /etc/pglord/tls/tls.crt"));
        assert!(ini.contains("client_tls_key_file = /etc/pglord/tls/tls.key"));
        assert!(ini.contains("server_tls_sslmode = require"));
    }

    #[test]
    fn userlist_holds_exactly_the_pooler_role() {
        assert_eq!(userlist("pw-123"), "\"pglord_pooler\" \"pw-123\"\n");
    }

    #[test]
    fn config_hash_changes_with_content() {
        let a = config_hash("ini-a", "ul");
        assert_eq!(a, config_hash("ini-a", "ul"), "deterministic");
        assert_ne!(a, config_hash("ini-b", "ul"));
        assert_ne!(a, config_hash("ini-a", "ul2"));
        assert_eq!(a.len(), 64);
    }
}
