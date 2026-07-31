//! Data-directory bootstrap: initdb for primaries, pg_basebackup for standbys.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::process::Command;

use crate::config::{Config, Role};

/// Extra postgresql.conf lines appended after initdb.
pub fn postgresql_conf_extra(port: u16) -> String {
    format!(
        "\n# --- managed by pglord-agent ---\n\
         listen_addresses = '*'\n\
         port = {port}\n\
         wal_level = replica\n\
         max_wal_senders = 10\n\
         hot_standby = on\n"
    )
}

/// Extra pg_hba.conf lines appended after initdb.
/// SECURITY: trust-everyone is acceptable ONLY for the isolated MVP e2e
/// network; the reconciler slice replaces this with scram + generated
/// credentials before anything faces a real cluster.
pub fn pg_hba_extra() -> String {
    "\n# --- managed by pglord-agent (MVP: trust; see SECURITY note) ---\n\
     host all all 0.0.0.0/0 trust\n\
     host replication all 0.0.0.0/0 trust\n"
        .to_string()
}

fn is_bootstrapped(pgdata: &Path) -> bool {
    pgdata.join("PG_VERSION").exists()
}

/// Idempotently prepare PGDATA for the configured role.
pub async fn ensure_bootstrapped(cfg: &Config) -> anyhow::Result<()> {
    if is_bootstrapped(&cfg.pgdata) {
        tracing::info!(pgdata = %cfg.pgdata.display(), "PGDATA already bootstrapped");
        return Ok(());
    }
    match cfg.role {
        Role::Primary => bootstrap_primary(cfg).await,
        Role::Standby => bootstrap_standby(cfg).await,
    }
}

async fn bootstrap_primary(cfg: &Config) -> anyhow::Result<()> {
    tracing::info!(pgdata = %cfg.pgdata.display(), "running initdb");
    let status = Command::new("initdb")
        .arg("-D")
        .arg(&cfg.pgdata)
        .args(["-A", "trust", "-U", &cfg.superuser])
        .status()
        .await
        .context("spawn initdb")?;
    if !status.success() {
        bail!("initdb failed with {status}");
    }

    append(
        &cfg.pgdata.join("postgresql.conf"),
        &postgresql_conf_extra(cfg.port),
    )?;
    append(&cfg.pgdata.join("pg_hba.conf"), &pg_hba_extra())?;
    tracing::info!("primary bootstrap complete");
    Ok(())
}

async fn bootstrap_standby(cfg: &Config) -> anyhow::Result<()> {
    if cfg.primary_host.is_empty() {
        bail!("standby role requires PGLORD_AGENT_PRIMARY_HOST");
    }
    // The primary may still be coming up; retry with backoff.
    for attempt in 1..=60u32 {
        tracing::info!(attempt, primary = %cfg.primary_host, "running pg_basebackup");
        let status = Command::new("pg_basebackup")
            .arg("-h")
            .arg(&cfg.primary_host)
            .arg("-p")
            .arg(cfg.primary_port.to_string())
            .arg("-U")
            .arg(&cfg.superuser)
            .arg("-D")
            .arg(&cfg.pgdata)
            .args(["-R", "-X", "stream"])
            .status()
            .await
            .context("spawn pg_basebackup")?;
        if status.success() {
            tracing::info!("standby bootstrap complete");
            return Ok(());
        }
        // A failed attempt can leave a partial PGDATA behind; start clean.
        if cfg.pgdata.exists() {
            std::fs::remove_dir_all(&cfg.pgdata).context("clean partial PGDATA")?;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("pg_basebackup did not succeed within the retry budget");
}

fn append(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("open {} for append", path.display()))?;
    f.write_all(content.as_bytes())
        .with_context(|| format!("append to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conf_extra_contains_replication_settings() {
        let conf = postgresql_conf_extra(5433);
        assert!(conf.contains("port = 5433"));
        assert!(conf.contains("wal_level = replica"));
        assert!(conf.contains("max_wal_senders = 10"));
        assert!(conf.contains("hot_standby = on"));
        assert!(conf.contains("listen_addresses = '*'"));
    }

    #[test]
    fn hba_extra_allows_replication() {
        let hba = pg_hba_extra();
        assert!(hba.contains("host replication all 0.0.0.0/0 trust"));
        assert!(hba.contains("host all all 0.0.0.0/0 trust"));
    }

    #[test]
    fn bootstrapped_detection_uses_pg_version() {
        let dir = std::env::temp_dir().join("pglord-agent-test-pgdata");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(!is_bootstrapped(&dir));
        std::fs::write(dir.join("PG_VERSION"), "17\n").expect("write");
        assert!(is_bootstrapped(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
