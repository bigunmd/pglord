//! Post-boot duties of a primary: idempotently ensure the replication role
//! exists with the configured password. Runs after postgres accepts
//! connections; safe to repeat on every startup.

use std::time::Duration;

use crate::config::Config;
use crate::status;

pub async fn ensure_replication_role(cfg: &Config) {
    if cfg.repl_password.is_empty() {
        tracing::warn!("no replication password configured; skipping role setup (trust mode)");
        return;
    }
    // Wait for postgres to accept connections (bounded).
    for _ in 0..60u32 {
        if status::query_status(cfg).await.ready {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    match try_ensure(cfg).await {
        Ok(()) => tracing::info!(role = %cfg.repl_user, "replication role ensured"),
        Err(err) => tracing::error!(%err, "failed to ensure replication role"),
    }
}

async fn try_ensure(cfg: &Config) -> anyhow::Result<()> {
    // Role name and password are interpolated into DDL below (Postgres cannot
    // bind them as parameters); enforce the charset invariant explicitly so a
    // misconfigured env var cannot smuggle SQL.
    if cfg.repl_user.is_empty()
        || !cfg
            .repl_user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        anyhow::bail!("repl_user must be non-empty alphanumeric/underscore");
    }
    // Charset must exclude '$' so the $pglord$ dollar-quote below cannot be
    // terminated from inside the password.
    if !cfg
        .repl_password
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("repl_password may only contain [A-Za-z0-9_-]");
    }
    let conn_str = format!(
        "host={} port={} user={} password={} dbname=postgres connect_timeout=5",
        cfg.socket_dir.display(),
        cfg.port,
        cfg.superuser,
        cfg.superuser_password,
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls).await?;
    let conn_task = tokio::spawn(connection);

    let exists: bool = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = $1)",
            &[&cfg.repl_user],
        )
        .await?
        .get(0);
    // Role names/passwords cannot be bound parameters; the role name comes
    // from our own config and the password is quoted via dollar-quoting with
    // a tag that cannot appear in the generated alphanumeric password.
    let sql = if exists {
        format!(
            "ALTER ROLE \"{}\" WITH REPLICATION LOGIN PASSWORD $pglord${}$pglord$",
            cfg.repl_user, cfg.repl_password
        )
    } else {
        format!(
            "CREATE ROLE \"{}\" WITH REPLICATION LOGIN PASSWORD $pglord${}$pglord$",
            cfg.repl_user, cfg.repl_password
        )
    };
    client.batch_execute(&sql).await?;
    drop(client);
    conn_task.abort();
    Ok(())
}
