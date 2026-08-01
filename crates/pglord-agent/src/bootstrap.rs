//! Data-directory bootstrap: a restore when the cluster was created from a
//! backup, otherwise initdb for primaries and pg_basebackup for standbys.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use pglord_core::restore::RestoreSpec;
use tokio::process::Command;

use crate::config::{Config, Role};

/// Written into PGDATA once a restore has finished completely. PG_VERSION
/// appears early in a restore, so it cannot tell a finished restore from one
/// that died half way; without this marker an interrupted restore would look
/// bootstrapped and crashloop forever on a partial data directory.
const RESTORE_MARKER: &str = "pglord.restore-complete";

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

/// Whether a restore still has to run: this cluster was created from a backup
/// and its data directory does not carry a completed-restore marker. An
/// unmarked but non-empty PGDATA is the wreckage of an interrupted restore.
pub fn restore_pending(pgdata: &Path, restore: Option<&RestoreSpec>) -> bool {
    restore.is_some() && !pgdata.join(RESTORE_MARKER).exists()
}

/// Idempotently prepare PGDATA for the configured role. A standby role with
/// primary-shaped data (no standby.signal) is a deposed ex-primary rejoining
/// after failover: rewind it BEFORE postgres starts — doing this at startup
/// (not at fence time) avoids racing the process supervisor.
pub async fn ensure_bootstrapped(
    cfg: &Config,
    restore: Option<&RestoreSpec>,
) -> anyhow::Result<()> {
    // Only the primary restores. Standbys of a restored cluster stream from it
    // with an ordinary basebackup, exactly like any other cluster's standbys.
    if cfg.role == Role::Primary && restore_pending(&cfg.pgdata, restore) {
        let restore = restore.expect("restore_pending implies Some");
        return bootstrap_restore(cfg, restore).await;
    }
    if is_bootstrapped(&cfg.pgdata) {
        if cfg.role == Role::Standby && !has_standby_signal(&cfg.pgdata) {
            tracing::warn!("ex-primary PGDATA with standby role: rewinding to new primary");
            return demote_to_standby(cfg).await;
        }
        if cfg.role == Role::Standby {
            let me = std::env::var("HOSTNAME").unwrap_or_default();
            refresh_conninfo(&cfg.pgdata, &primary_conninfo(cfg, &me))
                .context("refresh primary_conninfo")?;
            tracing::info!("primary_conninfo refreshed from environment");
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

/// Restore this cluster's PGDATA from another cluster's backup repository.
///
/// Runs before postgres has ever started, so nothing can be reading the data
/// directory while it is replaced.
async fn bootstrap_restore(cfg: &Config, r: &RestoreSpec) -> anyhow::Result<()> {
    tracing::info!(
        source = %r.source_cluster,
        label = r.backup_label.as_deref().unwrap_or("latest"),
        target_time = r.target_time.as_deref().unwrap_or("end of WAL"),
        "restoring from backup"
    );

    let conf = pglord_core::restore::pgbackrest_restore_conf(r, &cfg.pgdata.display().to_string());
    let conf_path = Path::new(pglord_core::backup::CONF_PATH);
    if let Some(dir) = conf_path.parent() {
        std::fs::create_dir_all(dir).context("create pgbackrest conf dir")?;
    }
    std::fs::write(conf_path, conf).context("write restore pgbackrest.conf")?;

    // Start from nothing. pgBackRest refuses to restore into a directory that
    // already has files, and leftovers here are always the remains of a failed
    // attempt — there is no data to preserve before the first successful boot.
    if cfg.pgdata.exists() {
        std::fs::remove_dir_all(&cfg.pgdata).context("clear PGDATA before restore")?;
    }
    std::fs::create_dir_all(&cfg.pgdata).context("create PGDATA")?;

    let mut cmd = Command::new("pgbackrest");
    cmd.arg(format!(
        "--stanza={}",
        pglord_core::restore::source_stanza(r)
    ))
    .args(pglord_core::restore::restore_args(r));
    // The repository being read is the source's, so these are the source
    // repository's credentials; they reach us through the same env vars.
    if !cfg.s3_access_key.is_empty() {
        cmd.env("PGBACKREST_REPO1_S3_KEY", &cfg.s3_access_key);
        cmd.env("PGBACKREST_REPO1_S3_KEY_SECRET", &cfg.s3_secret_key);
    }
    let out = cmd.output().await.context("spawn pgbackrest restore")?;
    if !out.status.success() {
        bail!(
            "restore from {} failed: {}",
            r.source_cluster,
            crate::backup::pgbackrest_error(&out.stdout, &out.stderr, out.status)
        );
    }

    // The restored PGDATA carries the SOURCE cluster's archive settings, whose
    // archive_command pushes WAL into the repository we just read from. Left
    // alone, this cluster would write its own diverging timeline into the
    // source's repository and corrupt the backups of a cluster that is very
    // possibly still running. Neutralise it before postgres ever starts;
    // enabling backups here later rewrites the file with our own stanza.
    crate::params::write_archive_if_changed(
        &cfg.pgdata,
        &crate::params::archive_conf(&cfg.cluster, false),
    )
    .context("disable inherited archiving")?;

    std::fs::write(cfg.pgdata.join(RESTORE_MARKER), "")
        .context("write restore completion marker")?;
    // Restored data is already an initialised database. Marking the init phase
    // done keeps init scripts — which create schemas and roles — from running
    // against tables that already hold the data being recovered.
    crate::initdb::mark_initialised(&cfg.pgdata).context("mark restored data as initialised")?;
    tracing::info!("restore complete; postgres will replay WAL and promote");
    Ok(())
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

/// Replace every primary_conninfo line in postgresql.auto.conf with `line`.
/// The password inside primary_conninfo is persisted at bootstrap; without
/// this refresh a standby restarted after a credentials rotation would keep
/// streaming with the old password forever.
pub fn refresh_conninfo(pgdata: &Path, line: &str) -> anyhow::Result<()> {
    let path = pgdata.join("postgresql.auto.conf");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut kept: String = existing
        .lines()
        .filter(|l| !l.trim_start().starts_with("primary_conninfo"))
        .map(|l| format!("{l}\n"))
        .collect();
    kept.push_str(line);
    std::fs::write(&path, kept).context("write postgresql.auto.conf")?;
    Ok(())
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
    fn refresh_conninfo_replaces_stale_lines_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join("pglord-agent-test-conninfo");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("postgresql.auto.conf"),
            "shared_buffers = '128MB'\n\
             primary_conninfo = 'host=old user=replicator password=OLD application_name=demo-1'\n\
             primary_conninfo = 'host=older user=replicator password=OLDER application_name=demo-1'\n",
        )
        .expect("seed");
        let fresh =
            "primary_conninfo = 'host=new user=replicator password=NEW application_name=demo-1'\n";
        refresh_conninfo(&dir, fresh).expect("refresh");
        let got = std::fs::read_to_string(dir.join("postgresql.auto.conf")).expect("read");
        assert!(got.contains("shared_buffers = '128MB'"));
        assert!(got.contains("password=NEW"));
        assert!(!got.contains("OLD"));
        assert_eq!(got.matches("primary_conninfo").count(), 1);
        // Idempotent: refreshing again changes nothing.
        refresh_conninfo(&dir, fresh).expect("refresh again");
        assert_eq!(
            got,
            std::fs::read_to_string(dir.join("postgresql.auto.conf")).expect("reread")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_conninfo_creates_the_file_when_missing() {
        let dir = std::env::temp_dir().join("pglord-agent-test-conninfo-new");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let fresh =
            "primary_conninfo = 'host=new user=replicator password=NEW application_name=demo-1'\n";
        refresh_conninfo(&dir, fresh).expect("refresh");
        let got = std::fs::read_to_string(dir.join("postgresql.auto.conf")).expect("read");
        assert_eq!(got, fresh);
        let _ = std::fs::remove_dir_all(&dir);
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

    fn restore_spec() -> RestoreSpec {
        serde_json::from_value(serde_json::json!({
            "source_cluster": "src",
            "repository": {
                "enabled": true,
                "s3_endpoint": "https://s3.example.com",
                "s3_bucket": "b",
                "s3_credentials_secret": "creds"
            }
        }))
        .expect("restore spec")
    }

    #[test]
    fn restore_runs_once_and_only_for_restored_clusters() {
        let dir = std::env::temp_dir().join("pglord-agent-test-restore");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let spec = restore_spec();

        // An ordinary cluster never restores, whatever is on disk.
        assert!(!restore_pending(&dir, None));

        // A restored cluster with an empty data directory has work to do.
        assert!(restore_pending(&dir, Some(&spec)));

        // Half-finished restore: PG_VERSION lands early, so it alone must not
        // count as done or the pod would crashloop on partial data forever.
        std::fs::write(dir.join("PG_VERSION"), "17\n").expect("write");
        assert!(
            restore_pending(&dir, Some(&spec)),
            "a partial restore must be redone, not treated as bootstrapped"
        );

        // Marked complete: never restore again, or every pod restart would wipe
        // the cluster back to the backup.
        std::fs::write(dir.join(RESTORE_MARKER), "").expect("write");
        assert!(!restore_pending(&dir, Some(&spec)));

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
