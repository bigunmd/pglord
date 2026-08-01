//! Prometheus metrics for one postgres instance, collected at scrape time.
//!
//! A fresh registry is built per scrape from a `MetricsSample`, so series for
//! things that went away (a departed standby, a dropped database) disappear
//! instead of going stale.

use prometheus::{Encoder, Gauge, GaugeVec, Opts, Registry, TextEncoder};

use crate::config::Config;

/// One collection pass. `Default` means "postgres unreachable".
#[derive(Debug, Default)]
pub struct MetricsSample {
    pub pg_up: bool,
    pub primary: Option<bool>,
    pub wal_lsn_bytes: Option<f64>,
    pub replication_lag_seconds: Option<f64>,
    /// (application_name, lag bytes) per pg_stat_replication row; primary only.
    pub standby_replay_lag_bytes: Vec<(String, f64)>,
    /// (state, count) from pg_stat_activity.
    pub connections: Vec<(String, f64)>,
    pub max_connections: Option<f64>,
    pub database_size_bytes: Vec<(String, f64)>,
    pub pending_restart: Option<bool>,
}

fn gauge(reg: &Registry, name: &str, help: &str, value: f64) {
    let g = Gauge::new(name, help).expect("static metric name");
    reg.register(Box::new(g.clone())).expect("unique metric name");
    g.set(value);
}

fn gauge_vec(reg: &Registry, name: &str, help: &str, label: &str, values: &[(String, f64)]) {
    if values.is_empty() {
        return;
    }
    let v = GaugeVec::new(Opts::new(name, help), &[label]).expect("static metric name");
    reg.register(Box::new(v.clone())).expect("unique metric name");
    for (key, value) in values {
        v.with_label_values(&[key]).set(*value);
    }
}

pub fn render(sample: &MetricsSample) -> String {
    let reg = Registry::new();
    gauge(
        &reg,
        "pglord_pg_up",
        "1 if the postgres stats query succeeded.",
        if sample.pg_up { 1.0 } else { 0.0 },
    );
    if let Some(primary) = sample.primary {
        gauge(
            &reg,
            "pglord_instance_primary",
            "1 on the primary, 0 on a standby.",
            if primary { 1.0 } else { 0.0 },
        );
    }
    if let Some(v) = sample.wal_lsn_bytes {
        gauge(
            &reg,
            "pglord_pg_wal_lsn_bytes",
            "Current WAL LSN as bytes: write position on the primary, replay position on a standby.",
            v,
        );
    }
    if let Some(v) = sample.replication_lag_seconds {
        gauge(
            &reg,
            "pglord_pg_replication_lag_seconds",
            "Standby only: seconds since the last replayed transaction.",
            v,
        );
    }
    gauge_vec(
        &reg,
        "pglord_pg_standby_replay_lag_bytes",
        "Primary only: replay lag in bytes per connected standby.",
        "standby",
        &sample.standby_replay_lag_bytes,
    );
    gauge_vec(
        &reg,
        "pglord_pg_connections",
        "Backends by state from pg_stat_activity.",
        "state",
        &sample.connections,
    );
    if let Some(v) = sample.max_connections {
        gauge(&reg, "pglord_pg_max_connections", "max_connections setting.", v);
    }
    gauge_vec(
        &reg,
        "pglord_pg_database_size_bytes",
        "Database size in bytes, non-template databases.",
        "database",
        &sample.database_size_bytes,
    );
    if let Some(v) = sample.pending_restart {
        gauge(
            &reg,
            "pglord_pg_pending_restart",
            "1 if settings are staged that need a restart.",
            if v { 1.0 } else { 0.0 },
        );
    }
    let mut buf = Vec::new();
    if TextEncoder::new().encode(&reg.gather(), &mut buf).is_err() {
        return String::new();
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// Collect a sample. Any failure returns the `Default` (pg_up = false) sample:
/// a scrape must never 5xx just because postgres is down.
pub async fn collect(cfg: &Config) -> MetricsSample {
    match try_collect(cfg).await {
        Ok(sample) => sample,
        Err(err) => {
            tracing::debug!(%err, "metrics collection failed");
            MetricsSample::default()
        }
    }
}

async fn try_collect(cfg: &Config) -> anyhow::Result<MetricsSample> {
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
             pg_wal_lsn_diff(CASE WHEN pg_is_in_recovery() \
                                  THEN pg_last_wal_replay_lsn() \
                                  ELSE pg_current_wal_lsn() END, '0/0')::float8, \
             CASE WHEN pg_is_in_recovery() \
                  THEN extract(epoch from now() - pg_last_xact_replay_timestamp())::float8 END, \
             EXISTS(SELECT 1 FROM pg_settings WHERE pending_restart), \
             current_setting('max_connections')::float8",
            &[],
        )
        .await?;
    let in_recovery: bool = row.get(0);
    let mut sample = MetricsSample {
        pg_up: true,
        primary: Some(!in_recovery),
        wal_lsn_bytes: row.get(1),
        replication_lag_seconds: row.get(2),
        max_connections: row.get(4),
        pending_restart: Some(row.get(3)),
        ..Default::default()
    };

    if !in_recovery {
        // pg_current_wal_lsn() errors during recovery, so primary only.
        for row in client
            .query(
                "SELECT application_name, \
                 pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn)::float8 \
                 FROM pg_stat_replication WHERE replay_lsn IS NOT NULL",
                &[],
            )
            .await?
        {
            sample.standby_replay_lag_bytes.push((row.get(0), row.get(1)));
        }
    }
    for row in client
        .query(
            "SELECT COALESCE(state, 'unknown'), count(*)::float8 \
             FROM pg_stat_activity GROUP BY 1",
            &[],
        )
        .await?
    {
        sample.connections.push((row.get(0), row.get(1)));
    }
    for row in client
        .query(
            "SELECT datname, pg_database_size(datname)::float8 \
             FROM pg_database WHERE NOT datistemplate",
            &[],
        )
        .await?
    {
        sample.database_size_bytes.push((row.get(0), row.get(1)));
    }
    drop(client);
    conn_task.abort();
    Ok(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_sample() -> MetricsSample {
        MetricsSample {
            pg_up: true,
            primary: Some(true),
            wal_lsn_bytes: Some(16_777_216.0),
            replication_lag_seconds: None,
            standby_replay_lag_bytes: vec![("demo-1".to_string(), 4096.0)],
            connections: vec![("active".to_string(), 3.0), ("idle".to_string(), 5.0)],
            max_connections: Some(100.0),
            database_size_bytes: vec![("postgres".to_string(), 8_192_000.0)],
            pending_restart: Some(false),
        }
    }

    #[test]
    fn a_healthy_primary_renders_every_series() {
        let out = render(&full_sample());
        assert!(out.contains("pglord_pg_up 1"));
        assert!(out.contains("pglord_instance_primary 1"));
        assert!(out.contains("pglord_pg_wal_lsn_bytes 16777216"));
        assert!(out.contains("pglord_pg_standby_replay_lag_bytes{standby=\"demo-1\"} 4096"));
        assert!(out.contains("pglord_pg_connections{state=\"active\"} 3"));
        assert!(out.contains("pglord_pg_max_connections 100"));
        assert!(out.contains("pglord_pg_database_size_bytes{database=\"postgres\"} 8192000"));
        assert!(out.contains("pglord_pg_pending_restart 0"));
    }

    #[test]
    fn a_standby_reports_lag_seconds_not_replication_rows() {
        let sample = MetricsSample {
            pg_up: true,
            primary: Some(false),
            replication_lag_seconds: Some(1.5),
            ..Default::default()
        };
        let out = render(&sample);
        assert!(out.contains("pglord_instance_primary 0"));
        assert!(out.contains("pglord_pg_replication_lag_seconds 1.5"));
        assert!(!out.contains("pglord_pg_standby_replay_lag_bytes"));
    }

    #[test]
    fn postgres_down_still_renders_up_zero_and_nothing_else() {
        let out = render(&MetricsSample::default());
        assert!(out.contains("pglord_pg_up 0"));
        assert!(!out.contains("pglord_instance_primary"));
        assert!(!out.contains("pglord_pg_wal_lsn_bytes"));
    }
}
