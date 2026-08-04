//! Blue-green major upgrade, agent half.
//!
//! Two roles live here. On the **source** (the cluster being upgraded) this
//! module fences writes for the cutover window and cleans up the publications
//! and slots the migration created. On the **target** (the new instance set,
//! marked by `PGLORD_AGENT_UPGRADE_SOURCE`) it bootstraps the primary as a
//! logical subscriber of the source, reports sync progress, and executes the
//! cutover.
//!
//! Every step is idempotent and marker-gated, because the level-triggered
//! control plane retries and a pod can restart at any point.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Terminating open client backends is what makes the fence a real boundary:
/// `default_transaction_read_only` only applies to transactions that start
/// after the reload, so a long-running writer would otherwise commit after we
/// had already measured the source's final WAL position.
pub(crate) const TERMINATE_WRITERS_SQL: &str = "SELECT pg_terminate_backend(pid) \
     FROM pg_stat_activity \
     WHERE pid <> pg_backend_pid() AND backend_type = 'client backend'";

/// Databases a migration covers: everything a user could have written to.
const USER_DATABASES_SQL: &str = "SELECT datname, pg_get_userbyid(datdba) FROM pg_database \
     WHERE datallowconn AND NOT datistemplate AND datname <> 'postgres' \
     ORDER BY datname";

const SLOT_LIKE: &str = r"pglord\_upgrade\_%";

/// How far each upgrade slot has confirmed relative to a target position.
///
/// The bind parameter is cast `text -> pg_lsn`, not straight to `pg_lsn`:
/// casting directly makes postgres infer the parameter's type as `pg_lsn`,
/// which the client cannot serialize a string into ("error serializing
/// parameter 0"). Going through text keeps the parameter a plain string.
fn drain_progress_sql() -> String {
    format!(
        "SELECT pg_wal_lsn_diff(confirmed_flush_lsn, $1::text::pg_lsn)::int8 \
         FROM pg_replication_slots WHERE slot_name LIKE '{SLOT_LIKE}'"
    )
}

// ---------------------------------------------------------------------------
// Source side
// ---------------------------------------------------------------------------

/// Make the source primary read-only for the cutover window.
///
/// `ALTER SYSTEM`, not a session setting: a pod restart during the soak must
/// come back still fenced, or applications that never noticed the cutover
/// would resume writing to the abandoned database.
pub async fn fence_writes(cfg: &Config) -> anyhow::Result<()> {
    let (client, _conn) = crate::postboot::connect_local(cfg).await?;
    client
        .batch_execute("ALTER SYSTEM SET default_transaction_read_only = on")
        .await?;
    client.batch_execute("SELECT pg_reload_conf()").await?;
    client.batch_execute(TERMINATE_WRITERS_SQL).await?;
    tracing::warn!("writes fenced for major-upgrade cutover");
    Ok(())
}

pub async fn unfence_writes(cfg: &Config) -> anyhow::Result<()> {
    let (client, _conn) = crate::postboot::connect_local(cfg).await?;
    client
        .batch_execute("ALTER SYSTEM RESET default_transaction_read_only")
        .await?;
    client.batch_execute("SELECT pg_reload_conf()").await?;
    tracing::warn!("writes unfenced");
    Ok(())
}

/// Drop this migration's publications and any inactive slot it left behind.
///
/// Idempotent, and called on both abort and rollback: an abandoned logical
/// slot pins WAL forever, which fills the volume of a cluster whose upgrade
/// the user thought they had cancelled.
pub async fn cleanup_source_artifacts(cfg: &Config) -> anyhow::Result<()> {
    let (client, _conn) = crate::postboot::connect_local(cfg).await?;
    let rows = client.query(USER_DATABASES_SQL, &[]).await?;
    for row in rows {
        let db: String = row.get(0);
        match crate::postboot::connect_local_db(cfg, &db).await {
            Ok((c, _h)) => {
                if let Err(err) = c
                    .batch_execute(&pglord_core::upgrade::drop_publication_sql())
                    .await
                {
                    tracing::warn!(%db, %err, "failed to drop upgrade publication");
                }
            }
            Err(err) => tracing::warn!(%db, %err, "failed to connect for publication cleanup"),
        }
    }
    let dropped = client
        .execute(
            &format!(
                "SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots \
                 WHERE slot_name LIKE '{SLOT_LIKE}' AND NOT active"
            ),
            &[],
        )
        .await?;
    tracing::info!(dropped, "upgrade artifacts cleaned up");
    Ok(())
}

// ---------------------------------------------------------------------------
// Target side
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbSync {
    pub name: String,
    pub tables_total: i64,
    pub tables_ready: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeAgentStatus {
    /// Every source database has a subscription here.
    pub established: bool,
    /// Cutover already ran on this instance set.
    pub complete: bool,
    pub databases: Vec<DbSync>,
    /// Worst-case bytes of source WAL not yet confirmed by our slots.
    pub lag_bytes: Option<i64>,
    pub error: Option<String>,
}

pub fn source_conninfo(cfg: &Config, db: &str) -> String {
    format!(
        "host={} port={} user={} password={} dbname={}",
        cfg.upgrade_source, cfg.primary_port, cfg.superuser, cfg.superuser_password, db
    )
}

fn marker_path(pgdata: &Path, step: &str) -> PathBuf {
    pgdata.join(format!("pglord.upgrade-{step}"))
}

pub(crate) fn marker_exists(pgdata: &Path, step: &str) -> bool {
    marker_path(pgdata, step).exists()
}

pub(crate) fn write_marker(pgdata: &Path, step: &str) -> std::io::Result<()> {
    std::fs::write(marker_path(pgdata, step), b"")
}

pub(crate) fn schema_marker(db: &str) -> String {
    format!("schema-{}", pglord_core::upgrade::subscription_name(db))
}

async fn connect_source(
    cfg: &Config,
    db: &str,
) -> anyhow::Result<(
    tokio_postgres::Client,
    tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
)> {
    let conn_str = format!("{} connect_timeout=5", source_conninfo(cfg, db));
    let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls).await?;
    Ok((client, tokio::spawn(connection)))
}

/// Double-quote an identifier for interpolation into DDL that cannot take a
/// bind parameter (CREATE DATABASE and friends).
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Establish the subscription, retrying until it sticks. Returns once the
/// migration is running; the control plane watches progress via `status`.
pub async fn run_subscriber(cfg: std::sync::Arc<Config>) {
    // A pod that restarts during the soak must not re-subscribe: the source is
    // fenced and detached by then, and re-creating subscriptions would point
    // this cluster back at a database nobody is writing to.
    if marker_exists(&cfg.pgdata, "complete") {
        tracing::info!("upgrade cutover already completed here; subscriber idle");
        return;
    }
    loop {
        match try_establish(&cfg).await {
            Ok(()) => {
                tracing::info!("upgrade subscriptions established");
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "upgrade subscriber not established yet; retrying");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn try_establish(cfg: &Config) -> anyhow::Result<()> {
    // Wait for our own postgres.
    if !crate::status::query_status(cfg).await.ready {
        anyhow::bail!("local postgres not ready");
    }
    let (source, _sh) = connect_source(cfg, "postgres").await?;

    // Direction guard. The server cannot judge an arbitrary image tag, so the
    // only place the two majors are both known for certain is here.
    let their_version: i32 = source
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .await?
        .get(0);
    let (local, _lh) = crate::postboot::connect_local(cfg).await?;
    let our_version: i32 = local
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .await?
        .get(0);
    if our_version / 10_000 < their_version / 10_000 {
        anyhow::bail!(
            "target runs PostgreSQL {} but the source runs {}; a major upgrade cannot go backwards",
            our_version / 10_000,
            their_version / 10_000
        );
    }

    // Globals (roles, and the grants between them) come from the source, not
    // from the declarative spec: a database created by hand outside pglord is
    // still owned by a role that has to exist here before its schema restores.
    if !marker_exists(&cfg.pgdata, "globals") {
        let dump = run_capture(
            cfg,
            "pg_dumpall",
            &[
                "--globals-only".to_string(),
                "-h".to_string(),
                cfg.upgrade_source.clone(),
                "-p".to_string(),
                cfg.primary_port.to_string(),
                "-U".to_string(),
                cfg.superuser.clone(),
            ],
        )
        .await?;
        // No ON_ERROR_STOP: roles this instance already created (the
        // replication role, the pooler role) collide harmlessly, and a retry
        // after a partial apply must be able to finish the rest.
        apply_sql(cfg, "postgres", &dump, false).await?;
        write_marker(&cfg.pgdata, "globals")?;
        tracing::info!("upgrade: globals restored from source");
    }

    let databases = source.query(USER_DATABASES_SQL, &[]).await?;
    for row in &databases {
        let db: String = row.get(0);
        let owner: String = row.get(1);

        let exists: bool = local
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)",
                &[&db],
            )
            .await?
            .get(0);
        if !exists {
            local
                .batch_execute(&format!(
                    "CREATE DATABASE {} OWNER {}",
                    quote_ident(&db),
                    quote_ident(&owner)
                ))
                .await?;
            tracing::info!(%db, "upgrade: database created");
        }

        if !marker_exists(&cfg.pgdata, &schema_marker(&db)) {
            // The new major's pg_dump reading an older server is the supported
            // direction; the reverse is not, which is why this runs here.
            let schema = run_capture(
                cfg,
                "pg_dump",
                &[
                    "--schema-only".to_string(),
                    "--no-publications".to_string(),
                    "--no-subscriptions".to_string(),
                    "-h".to_string(),
                    cfg.upgrade_source.clone(),
                    "-p".to_string(),
                    cfg.primary_port.to_string(),
                    "-U".to_string(),
                    cfg.superuser.clone(),
                    db.clone(),
                ],
            )
            .await?;
            apply_sql(cfg, &db, &schema, true).await?;
            write_marker(&cfg.pgdata, &schema_marker(&db))?;
            tracing::info!(%db, "upgrade: schema restored");
        }

        let (src_db, _h) = connect_source(cfg, &db).await?;
        let has_pub: bool = src_db
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_publication WHERE pubname = $1)",
                &[&pglord_core::upgrade::PUBLICATION],
            )
            .await?
            .get(0);
        if !has_pub {
            src_db
                .batch_execute(&pglord_core::upgrade::create_publication_sql())
                .await?;
            tracing::info!(%db, "upgrade: publication created on source");
        }

        let sub = pglord_core::upgrade::subscription_name(&db);
        let (local_db, _lh) = crate::postboot::connect_local_db(cfg, &db).await?;
        let has_sub: bool = local_db
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_subscription WHERE subname = $1)",
                &[&sub],
            )
            .await?
            .get(0);
        if !has_sub {
            local_db
                .batch_execute(&pglord_core::upgrade::create_subscription_sql(
                    &db,
                    &source_conninfo(cfg, &db),
                ))
                .await?;
            tracing::info!(%db, %sub, "upgrade: subscription created");
        }
    }
    Ok(())
}

/// Run a client binary against the source, returning its stdout.
async fn run_capture(cfg: &Config, program: &str, args: &[String]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .env("PGPASSWORD", &cfg.superuser_password)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Feed SQL to the local server through psql over the unix socket.
async fn apply_sql(cfg: &Config, db: &str, sql: &str, stop_on_error: bool) -> anyhow::Result<()> {
    let file = cfg.pgdata.join(format!("pglord.upgrade-{db}.sql"));
    std::fs::write(&file, sql)?;
    let mut cmd = tokio::process::Command::new("psql");
    cmd.args([
        "-U",
        &cfg.superuser,
        "-h",
        &cfg.socket_dir.to_string_lossy(),
        "-p",
        &cfg.port.to_string(),
        "-d",
        db,
        "--no-psqlrc",
    ]);
    if stop_on_error {
        cmd.args(["-v", "ON_ERROR_STOP=1"]);
    }
    cmd.arg("-f").arg(&file);
    cmd.env("PGPASSWORD", &cfg.superuser_password);
    let out = cmd.output().await?;
    let _ = std::fs::remove_file(&file);
    if !out.status.success() {
        anyhow::bail!(
            "psql on {db} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Every upgrade slot has confirmed at least the target position. An empty
/// list is *not* drained: it means nothing is subscribed.
pub(crate) fn drained(diffs: &[i64]) -> bool {
    !diffs.is_empty() && diffs.iter().all(|d| *d >= 0)
}

pub async fn status(cfg: &Config) -> UpgradeAgentStatus {
    let complete = marker_exists(&cfg.pgdata, "complete");
    let mut out = UpgradeAgentStatus {
        established: false,
        complete,
        databases: Vec::new(),
        lag_bytes: None,
        error: None,
    };
    let (source, _sh) = match connect_source(cfg, "postgres").await {
        Ok(c) => c,
        Err(err) => {
            // After cutover the source is detached on purpose; that is not an
            // error worth surfacing.
            if !complete {
                out.error = Some(format!("source unreachable: {err}"));
            }
            return out;
        }
    };
    let source_dbs = match source.query(USER_DATABASES_SQL, &[]).await {
        Ok(rows) => rows
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>(),
        Err(err) => {
            out.error = Some(format!("source database list failed: {err}"));
            return out;
        }
    };

    let mut all_subscribed = true;
    for db in &source_dbs {
        let sub = pglord_core::upgrade::subscription_name(db);
        let Ok((local_db, _h)) = crate::postboot::connect_local_db(cfg, db).await else {
            all_subscribed = false;
            continue;
        };
        match local_db
            .query_one(
                "SELECT COALESCE(COUNT(*), 0)::int8, \
                 COALESCE(COUNT(*) FILTER (WHERE r.srsubstate = 'r'), 0)::int8 \
                 FROM pg_subscription s LEFT JOIN pg_subscription_rel r ON r.srsubid = s.oid \
                 WHERE s.subname = $1",
                &[&sub],
            )
            .await
        {
            Ok(row) => {
                let total: i64 = row.get(0);
                let ready: i64 = row.get(1);
                out.databases.push(DbSync {
                    name: db.clone(),
                    tables_total: total,
                    tables_ready: ready,
                });
            }
            Err(_) => all_subscribed = false,
        }
        let subscribed: bool = local_db
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_subscription WHERE subname = $1)",
                &[&sub],
            )
            .await
            .map(|r| r.get(0))
            .unwrap_or(false);
        if !subscribed {
            all_subscribed = false;
        }
    }
    out.established = all_subscribed;

    out.lag_bytes = source
        .query_one(
            &format!(
                "SELECT COALESCE(MAX(pg_wal_lsn_diff(pg_current_wal_lsn(), \
                 confirmed_flush_lsn)), 0)::int8 \
                 FROM pg_replication_slots WHERE slot_name LIKE '{SLOT_LIKE}'"
            ),
            &[],
        )
        .await
        .ok()
        .map(|r| r.get(0));
    out
}

/// Drain, sync sequences, detach. Leaves this instance set holding every
/// committed row of the source; the control plane flips the primary record
/// after this returns.
pub async fn cutover(cfg: &Config) -> anyhow::Result<()> {
    if marker_exists(&cfg.pgdata, "complete") {
        return Ok(()); // idempotent: a retried cutover is a no-op
    }
    let st = status(cfg).await;
    if !st.established {
        anyhow::bail!("subscriptions not established; refusing to cut over");
    }
    let (source, _sh) = connect_source(cfg, "postgres").await?;
    let dbs = source.query(USER_DATABASES_SQL, &[]).await?;

    // A cluster with no user databases has no subscriptions and so no slots to
    // wait on. Draining an empty set would time out forever, because "no
    // slots" otherwise means "nothing is subscribed".
    if !dbs.is_empty() {
        let target_lsn: String = source
            .query_one("SELECT pg_current_wal_lsn()::text", &[])
            .await?
            .get(0);

        // The source is already fenced by the control plane, so its write
        // position is final; wait for our slots to confirm it.
        let mut drained_ok = false;
        for _ in 0..30u32 {
            let diffs: Vec<i64> = source
                .query(&drain_progress_sql(), &[&target_lsn])
                .await?
                .iter()
                .map(|r| r.get::<_, i64>(0))
                .collect();
            if drained(&diffs) {
                drained_ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if !drained_ok {
            anyhow::bail!("drain timed out; the source still has unreplicated WAL");
        }
    }

    for row in &dbs {
        let db: String = row.get(0);
        // Sequences are not replicated logically: without this every INSERT
        // after cutover would collide with existing rows.
        let (src_db, _h) = connect_source(cfg, &db).await?;
        let seqs = src_db
            .query(
                "SELECT schemaname, sequencename, COALESCE(last_value, 0) \
                 FROM pg_sequences",
                &[],
            )
            .await?;
        let (local_db, _lh) = crate::postboot::connect_local_db(cfg, &db).await?;
        for seq in &seqs {
            let schema: String = seq.get(0);
            let name: String = seq.get(1);
            let last: i64 = seq.get(2);
            // The margin covers values the source handed out from its cache
            // and never wrote to WAL.
            if let Err(err) = local_db
                .execute(
                    "SELECT setval(format('%I.%I', $1::text, $2::text)::regclass, $3::int8, true)",
                    &[&schema, &name, &(last + 1000)],
                )
                .await
            {
                tracing::warn!(%db, %schema, %name, %err, "sequence sync failed");
            }
        }

        // Drop the subscription while the source is still reachable — that is
        // what removes its replication slot rather than orphaning it.
        local_db
            .batch_execute(&pglord_core::upgrade::drop_subscription_sql(&db))
            .await?;
        src_db
            .batch_execute(&pglord_core::upgrade::drop_publication_sql())
            .await?;
        tracing::info!(%db, "upgrade: detached from source");
    }
    write_marker(&cfg.pgdata, "complete")?;
    tracing::warn!("upgrade cutover complete on this instance set");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        serde_json::from_value(serde_json::json!({ "role": "primary" })).expect("config")
    }

    #[test]
    fn terminate_sql_spares_our_own_backend() {
        assert!(TERMINATE_WRITERS_SQL.contains("pid <> pg_backend_pid()"));
        // Only client sessions: killing background workers (the walsenders
        // feeding the migration among them) would break the drain.
        assert!(TERMINATE_WRITERS_SQL.contains("client backend"));
    }

    #[test]
    fn drain_parameter_is_cast_through_text() {
        // Binding straight to pg_lsn makes postgres infer that type for the
        // parameter, and the client cannot serialize a string into it — the
        // cutover then fails with "error serializing parameter 0".
        let sql = drain_progress_sql();
        assert!(sql.contains("$1::text::pg_lsn"), "{sql}");
        assert!(!sql.contains("$1::pg_lsn"), "{sql}");
    }

    #[test]
    fn drained_requires_every_slot_at_or_past_the_target() {
        assert!(drained(&[0, 5]));
        assert!(!drained(&[-1, 5]));
        // No slots at all means nothing is subscribed: never call that drained.
        assert!(!drained(&[]));
    }

    #[test]
    fn source_conninfo_targets_the_configured_host_and_database() {
        let mut c = cfg();
        c.upgrade_source = "demo-rw.pglord.svc.cluster.local".into();
        c.superuser_password = "pw".into();
        let ci = source_conninfo(&c, "appdb");
        assert!(ci.contains("host=demo-rw.pglord.svc.cluster.local"), "{ci}");
        assert!(ci.contains("dbname=appdb"), "{ci}");
        assert!(ci.contains("user=postgres"), "{ci}");
    }

    #[test]
    fn markers_gate_each_step_independently() {
        let dir = std::env::temp_dir().join("pglord-agent-test-upgrade-markers");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(!marker_exists(&dir, "globals"));
        write_marker(&dir, "globals").expect("write");
        assert!(marker_exists(&dir, "globals"));
        // Per-database markers are sanitized like subscription names, so an
        // exotic database name cannot escape into the path.
        let m = schema_marker("My-DB/1");
        assert_eq!(m, "schema-pglord_upgrade_my_db_1");
        assert!(!marker_exists(&dir, &m));
        write_marker(&dir, &m).expect("write");
        assert!(marker_exists(&dir, &m));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identifiers_are_quoted_against_injection() {
        assert_eq!(quote_ident("appdb"), "\"appdb\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}
