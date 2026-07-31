//! Data-directory bootstrap: initdb for primaries, pg_basebackup for standbys.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::process::Command;

use crate::config::{Config, Role};

/// Extra postgresql.conf lines appended after initdb.
/// wal_log_hints is required for pg_rewind (old-primary rejoin after failover).
///
/// Order matters: the user parameter file is included first, then the managed
/// settings are repeated, so a user parameter can never override the settings
/// HA depends on (last assignment wins in postgresql.conf).
pub fn postgresql_conf_extra(port: u16) -> String {
    format!(
        "\n# --- managed by pglord-agent ---\n\
         include_if_exists = '{}'\n\
         include_if_exists = '{}'\n\
         listen_addresses = '*'\n\
         port = {port}\n\
         wal_level = replica\n\
         max_wal_senders = 10\n\
         hot_standby = on\n\
         wal_log_hints = on\n",
        crate::params::PARAMS_FILE,
        crate::params::ARCHIVE_FILE,
    )
}

/// Extra pg_hba.conf lines appended after initdb. With a password configured,
/// network access is scram-only; the trust variant remains ONLY for
/// credential-less dev runs and is loudly logged.
pub fn pg_hba_extra(repl_user: &str, scram: bool) -> String {
    if scram {
        format!(
            "\n# --- managed by pglord-agent ---\n\
             host all all 0.0.0.0/0 scram-sha-256\n\
             host replication {repl_user} 0.0.0.0/0 scram-sha-256\n"
        )
    } else {
        "\n# --- managed by pglord-agent (INSECURE trust mode: no password configured) ---\n\
         host all all 0.0.0.0/0 trust\n\
         host replication all 0.0.0.0/0 trust\n"
            .to_string()
    }
}

fn is_bootstrapped(pgdata: &Path) -> bool {
    pgdata.join("PG_VERSION").exists()
}

fn has_standby_signal(pgdata: &Path) -> bool {
    pgdata.join("standby.signal").exists()
}

/// Idempotently prepare PGDATA for the configured role. A standby role with
/// primary-shaped data (no standby.signal) is a deposed ex-primary rejoining
/// after failover: rewind it BEFORE postgres starts — doing this at startup
/// (not at fence time) avoids racing the process supervisor.
pub async fn ensure_bootstrapped(cfg: &Config) -> anyhow::Result<()> {
    if is_bootstrapped(&cfg.pgdata) {
        if cfg.role == Role::Standby && !has_standby_signal(&cfg.pgdata) {
            tracing::warn!("ex-primary PGDATA with standby role: rewinding to new primary");
            return demote_to_standby(cfg).await;
        }
        tracing::info!(pgdata = %cfg.pgdata.display(), "PGDATA already bootstrapped");
        return Ok(());
    }
    match cfg.role {
        Role::Primary => bootstrap_primary(cfg).await,
        Role::Standby => bootstrap_standby(cfg).await,
        Role::Auto => bail!("role must be resolved before bootstrap (see rolestrategy)"),
    }
}

/// Connection user/password for streaming: replication role when configured,
/// superuser in trust-mode dev runs.
fn stream_credentials(cfg: &Config) -> (&str, &str) {
    if cfg.repl_password.is_empty() {
        (cfg.superuser.as_str(), cfg.superuser_password.as_str())
    } else {
        (cfg.repl_user.as_str(), cfg.repl_password.as_str())
    }
}

/// The primary_conninfo line a rejoining standby streams with.
pub fn primary_conninfo(cfg: &Config, application_name: &str) -> String {
    let (user, password) = stream_credentials(cfg);
    format!(
        "primary_conninfo = 'host={} port={} user={user} password={password} application_name={application_name}'\n",
        cfg.primary_host, cfg.primary_port,
    )
}

/// pg_rewind against the current primary; fall back to a fresh basebackup if
/// the rewind fails (e.g. missing WAL).
async fn demote_to_standby(cfg: &Config) -> anyhow::Result<()> {
    let source = format!(
        "host={} port={} user={} password={} dbname=postgres",
        cfg.primary_host, cfg.primary_port, cfg.superuser, cfg.superuser_password,
    );
    let rewound = Command::new("pg_rewind")
        .arg(format!("--target-pgdata={}", cfg.pgdata.display()))
        .arg(format!("--source-server={source}"))
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);

    if rewound {
        let me = std::env::var("HOSTNAME").unwrap_or_default();
        append(
            &cfg.pgdata.join("postgresql.auto.conf"),
            &primary_conninfo(cfg, &me),
        )?;
        std::fs::write(cfg.pgdata.join("standby.signal"), "").context("write standby.signal")?;
        tracing::info!("rewind complete; prepared as standby");
        return Ok(());
    }

    tracing::warn!("pg_rewind failed; wiping PGDATA for a fresh basebackup");
    std::fs::remove_dir_all(&cfg.pgdata).context("wipe PGDATA")?;
    bootstrap_standby(cfg).await
}

async fn bootstrap_primary(cfg: &Config) -> anyhow::Result<()> {
    tracing::info!(pgdata = %cfg.pgdata.display(), "running initdb");
    let scram = !cfg.superuser_password.is_empty();
    let mut cmd = Command::new("initdb");
    cmd.arg("-D").arg(&cfg.pgdata).args(["-U", &cfg.superuser]);

    // Password file lives outside PGDATA and is removed right after initdb.
    // create_new + 0600 defeats symlink attacks and other-user reads.
    let pwfile = std::env::temp_dir().join(format!("pglord-initdb-pw.{}", std::process::id()));
    if scram {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pwfile)
            .context("create initdb pwfile")?;
        writeln!(f, "{}", cfg.superuser_password).context("write initdb pwfile")?;
        drop(f);
        // Network connections need a password; local socket connections use
        // peer auth so tools running as the postgres OS user (pgBackRest,
        // pg_ctl, our own status queries) can connect without one.
        cmd.args(["--auth-local", "peer", "--auth-host", "scram-sha-256"])
            .arg("--pwfile")
            .arg(&pwfile);
    } else {
        tracing::warn!("no superuser password configured: bootstrapping in INSECURE trust mode");
        cmd.args(["-A", "trust"]);
    }
    let status = cmd.status().await.context("spawn initdb")?;
    let _ = std::fs::remove_file(&pwfile);
    if !status.success() {
        bail!("initdb failed with {status}");
    }

    append(
        &cfg.pgdata.join("postgresql.conf"),
        &postgresql_conf_extra(cfg.port),
    )?;
    append(
        &cfg.pgdata.join("pg_hba.conf"),
        &pg_hba_extra(&cfg.repl_user, scram),
    )?;
    tracing::info!(scram, "primary bootstrap complete");
    Ok(())
}

async fn bootstrap_standby(cfg: &Config) -> anyhow::Result<()> {
    if cfg.primary_host.is_empty() {
        bail!("standby role requires PGLORD_AGENT_PRIMARY_HOST");
    }
    // The primary may still be coming up; retry with backoff.
    let scram = !cfg.repl_password.is_empty();
    // With credentials configured, stream as the replication role; trust-mode
    // dev runs fall back to the superuser.
    let (user, password) = if scram {
        (cfg.repl_user.as_str(), cfg.repl_password.as_str())
    } else {
        (cfg.superuser.as_str(), "")
    };
    for attempt in 1..=60u32 {
        tracing::info!(attempt, primary = %cfg.primary_host, user, "running pg_basebackup");
        let status = Command::new("pg_basebackup")
            .arg("-h")
            .arg(&cfg.primary_host)
            .arg("-p")
            .arg(cfg.primary_port.to_string())
            .arg("-U")
            .arg(user)
            .arg("-D")
            .arg(&cfg.pgdata)
            .args(["-R", "-X", "stream"])
            .env("PGPASSWORD", password)
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
        assert!(conf.contains("wal_log_hints = on"));
        // The include must come before the managed settings so user parameters
        // cannot override them.
        let include_at = conf.find("include_if_exists").expect("include line");
        let wal_level_at = conf.find("wal_level").expect("wal_level line");
        assert!(include_at < wal_level_at);
    }

    #[test]
    fn hba_scram_mode_has_no_trust_lines() {
        let hba = pg_hba_extra("replicator", true);
        assert!(hba.contains("host all all 0.0.0.0/0 scram-sha-256"));
        assert!(hba.contains("host replication replicator 0.0.0.0/0 scram-sha-256"));
        assert!(!hba.contains("trust"));
    }

    #[test]
    fn hba_trust_fallback_kept_for_dev() {
        let hba = pg_hba_extra("replicator", false);
        assert!(hba.contains("host replication all 0.0.0.0/0 trust"));
        assert!(hba.contains("INSECURE"));
    }

    fn cfg_with(repl_password: &str) -> Config {
        let mut cfg: Config =
            serde_json::from_value(serde_json::json!({ "role": "primary" })).expect("config");
        cfg.primary_host = "demo-1.demo-pods.ns.svc.cluster.local".into();
        cfg.superuser_password = "supersecret".into();
        cfg.repl_password = repl_password.into();
        cfg
    }

    #[test]
    fn conninfo_streams_as_replication_role_when_configured() {
        // The rejoin path depends on this: a rewound ex-primary must stream
        // with the replication role, matching the scram pg_hba rule.
        let conninfo = primary_conninfo(&cfg_with("replsecret"), "demo-0");
        assert!(conninfo.contains("user=replicator"));
        assert!(conninfo.contains("password=replsecret"));
        assert!(conninfo.contains("host=demo-1.demo-pods.ns.svc.cluster.local"));
        assert!(conninfo.contains("application_name=demo-0"));
        assert!(conninfo.starts_with("primary_conninfo = '"));
    }

    #[test]
    fn conninfo_falls_back_to_superuser_without_replication_password() {
        let conninfo = primary_conninfo(&cfg_with(""), "demo-0");
        assert!(conninfo.contains("user=postgres"));
        assert!(conninfo.contains("password=supersecret"));
    }

    #[test]
    fn standby_signal_detection() {
        let dir = std::env::temp_dir().join("pglord-agent-test-signal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(!has_standby_signal(&dir));
        std::fs::write(dir.join("standby.signal"), "").expect("write");
        assert!(has_standby_signal(&dir));
        let _ = std::fs::remove_dir_all(&dir);
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
