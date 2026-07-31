//! Instance status via a local superuser connection over the unix socket.

use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Serialize)]
pub struct InstanceStatus {
    /// Effective role right now (post-promotion a bootstrapped standby
    /// reports primary).
    pub role: &'static str,
    /// Postgres accepts local connections.
    pub ready: bool,
    pub in_recovery: Option<bool>,
    /// Current WAL LSN: write position on a primary, receive position on a
    /// standby.
    pub lsn: Option<String>,
}

pub async fn query_status(cfg: &Config) -> InstanceStatus {
    match try_query(cfg).await {
        Ok((in_recovery, lsn)) => InstanceStatus {
            role: if in_recovery { "standby" } else { "primary" },
            ready: true,
            in_recovery: Some(in_recovery),
            lsn,
        },
        Err(err) => {
            tracing::debug!(%err, "status query failed");
            InstanceStatus {
                role: "unknown",
                ready: false,
                in_recovery: None,
                lsn: None,
            }
        }
    }
}

async fn try_query(cfg: &Config) -> anyhow::Result<(bool, Option<String>)> {
    let mut conn_str = format!(
        "host={} port={} user={} dbname=postgres connect_timeout=2",
        cfg.socket_dir.display(),
        cfg.port,
        cfg.superuser
    );
    if !cfg.superuser_password.is_empty() {
        conn_str.push_str(&format!(" password={}", cfg.superuser_password));
    }
    let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls).await?;
    let conn_task = tokio::spawn(connection);

    let row = client
        .query_one(
            "SELECT pg_is_in_recovery(), \
             CASE WHEN pg_is_in_recovery() \
                  THEN pg_last_wal_receive_lsn()::text \
                  ELSE pg_current_wal_lsn()::text END",
            &[],
        )
        .await?;
    let in_recovery: bool = row.get(0);
    let lsn: Option<String> = row.get(1);
    drop(client);
    conn_task.abort();
    Ok((in_recovery, lsn))
}
