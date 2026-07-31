//! Agent configuration. Layered: serde defaults <- config.toml <- PGLORD_AGENT_* env.

use std::net::SocketAddr;
use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Primary,
    Standby,
    /// Derive from the StatefulSet pod ordinal: 0 = primary, else standby.
    Auto,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Instance role at bootstrap. The reconciler sets this per pod; promotion
    /// can change the effective role at runtime.
    pub role: Role,
    /// PGDATA directory the agent owns.
    #[serde(default = "default_pgdata")]
    pub pgdata: PathBuf,
    /// Postgres listen port.
    #[serde(default = "default_pg_port")]
    pub port: u16,
    /// Agent control endpoint (internal; not part of the public API contract).
    #[serde(default = "default_http_addr")]
    pub http_addr: SocketAddr,
    /// Unix socket directory used for local superuser connections.
    #[serde(default = "default_socket_dir")]
    pub socket_dir: PathBuf,
    /// Primary host a standby bootstraps and streams from.
    #[serde(default = "default_empty")]
    pub primary_host: String,
    #[serde(default = "default_pg_port")]
    pub primary_port: u16,
    #[serde(default = "default_superuser")]
    pub superuser: String,
    /// Superuser password. Empty = trust auth (dev/e2e only).
    #[serde(default = "default_empty")]
    pub superuser_password: String,
    /// Replication role name; created idempotently by the primary after boot.
    #[serde(default = "default_repl_user")]
    pub repl_user: String,
    /// Replication password. Empty = trust auth (dev/e2e only).
    #[serde(default = "default_empty")]
    pub repl_password: String,
}

fn default_repl_user() -> String {
    "replicator".into()
}

fn default_pgdata() -> PathBuf {
    PathBuf::from("/var/lib/postgresql/data/pgdata")
}
fn default_pg_port() -> u16 {
    5432
}
fn default_http_addr() -> SocketAddr {
    "0.0.0.0:8008".parse().expect("valid literal")
}
fn default_socket_dir() -> PathBuf {
    PathBuf::from("/var/run/postgresql")
}
fn default_empty() -> String {
    String::new()
}
fn default_superuser() -> String {
    "postgres".into()
}

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        Ok(Figment::new()
            .merge(Toml::file("config.toml"))
            .merge(Env::prefixed("PGLORD_AGENT_"))
            .extract()?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::result_large_err)]
    use super::*;

    #[test]
    fn defaults_with_role_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AGENT_ROLE", "primary");
            let cfg = Config::load().expect("must load");
            assert_eq!(cfg.role, Role::Primary);
            assert_eq!(cfg.port, 5432);
            assert_eq!(cfg.http_addr.to_string(), "0.0.0.0:8008");
            assert_eq!(cfg.superuser, "postgres");
            Ok(())
        });
    }

    #[test]
    fn standby_env_overrides() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AGENT_ROLE", "standby");
            jail.set_env("PGLORD_AGENT_PRIMARY_HOST", "pg-primary");
            jail.set_env("PGLORD_AGENT_PGDATA", "/tmp/pgdata");
            let cfg = Config::load().expect("must load");
            assert_eq!(cfg.role, Role::Standby);
            assert_eq!(cfg.primary_host, "pg-primary");
            assert_eq!(cfg.pgdata, PathBuf::from("/tmp/pgdata"));
            Ok(())
        });
    }

    #[test]
    fn missing_role_is_rejected() {
        figment::Jail::expect_with(|_jail| {
            assert!(Config::load().is_err());
            Ok(())
        });
    }

    #[test]
    fn invalid_role_is_rejected() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_AGENT_ROLE", "leader");
            assert!(Config::load().is_err());
            Ok(())
        });
    }
}
