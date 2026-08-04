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
    /// Postgres has settings staged that only a restart will apply. Read from
    /// pg_settings, so it reflects reality rather than our own bookkeeping.
    pub pending_restart: bool,
    /// Primary only: connected standbys currently eligible to confirm a
    /// synchronous commit (pg_stat_replication sync_state 'sync'/'quorum').
    /// None on standbys. 0 with the sync flag on means writes are blocking.
    pub sync_standby_count: Option<u32>,
    /// The server is accepting TLS connections (`current_setting('ssl')`).
    /// None when postgres is unreachable.
    pub ssl: Option<bool>,
    /// Live `wal_level`. A major upgrade needs "logical" on the source, and
    /// the level only changes at server start — so this is what tells the
    /// server whether a rolling restart is required first.
    pub wal_level: Option<String>,
}

/// Process-wide fenced flag: set at boot when the spec lists this instance,
/// so `/status` reports the truth while postgres is deliberately down.
static FENCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_fenced(v: bool) {
    FENCED.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_fenced() -> bool {
    FENCED.load(std::sync::atomic::Ordering::Relaxed)
}

pub async fn query_status(cfg: &Config) -> InstanceStatus {
    if is_fenced() {
        // Postgres is stopped on purpose; a connection attempt would only
        // produce a misleading "unknown".
        return InstanceStatus {
            role: "fenced",
            ready: false,
            in_recovery: None,
            lsn: None,
            pending_restart: false,
            sync_standby_count: None,
            ssl: None,
            wal_level: None,
        };
    }
    match try_query(cfg).await {
        Ok((in_recovery, lsn, pending_restart, sync_standby_count, ssl, wal_level)) => {
            InstanceStatus {
                role: if in_recovery { "standby" } else { "primary" },
                ready: true,
                in_recovery: Some(in_recovery),
                lsn,
                pending_restart,
                sync_standby_count,
                ssl: Some(ssl),
                wal_level: Some(wal_level),
            }
        }
        Err(err) => {
            tracing::debug!(%err, "status query failed");
            InstanceStatus {
                role: "unknown",
                ready: false,
                in_recovery: None,
                lsn: None,
                pending_restart: false,
                sync_standby_count: None,
                ssl: None,
                wal_level: None,
            }
        }
    }
}

#[allow(clippy::type_complexity)]
async fn try_query(
    cfg: &Config,
) -> anyhow::Result<(bool, Option<String>, bool, Option<u32>, bool, String)> {
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
                  ELSE pg_current_wal_lsn()::text END, \
             EXISTS(SELECT 1 FROM pg_settings WHERE pending_restart), \
             CASE WHEN pg_is_in_recovery() THEN NULL \
                  ELSE (SELECT count(*)::int4 FROM pg_stat_replication \
                        WHERE sync_state IN ('sync', 'quorum')) END, \
             current_setting('ssl') = 'on', \
             current_setting('wal_level')",
            &[],
        )
        .await?;
    let in_recovery: bool = row.get(0);
    let lsn: Option<String> = row.get(1);
    let pending_restart: bool = row.get(2);
    let sync_standby_count = row
        .get::<_, Option<i32>>(3)
        .map(|c| u32::try_from(c).unwrap_or(0));
    let ssl: bool = row.get(4);
    let wal_level: String = row.get(5);
    drop(client);
    conn_task.abort();
    Ok((
        in_recovery,
        lsn,
        pending_restart,
        sync_standby_count,
        ssl,
        wal_level,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fenced_flag_overrides_status() {
        let cfg: Config = serde_json::from_value(serde_json::json!({ "role": "standby" }))
            .expect("config from defaults");
        set_fenced(true);
        let st = query_status(&cfg).await;
        assert_eq!(st.role, "fenced");
        assert!(!st.ready);
        assert_eq!(st.in_recovery, None);
        set_fenced(false);
    }
}
