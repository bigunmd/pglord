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
         include_if_exists = '{}'\n\
         include_if_exists = '{}'\n\
         include_if_exists = '{}'\n\
         listen_addresses = '*'\n\
         port = {port}\n\
         wal_level = logical\n\
         max_wal_senders = 10\n\
         hot_standby = on\n\
         wal_log_hints = on\n",
        crate::params::PARAMS_FILE,
        crate::params::ARCHIVE_FILE,
        crate::params::SYNC_FILE,
        crate::params::STANDBY_FILE,
        crate::params::TLS_FILE,
    )
}

fn is_bootstrapped(pgdata: &Path) -> bool {
    pgdata.join("PG_VERSION").exists()
}

/// Bring a PGDATA created by an older agent up to `wal_level = logical`.
///
/// Logical decoding is what makes a major-version upgrade possible at all
/// (the new instances subscribe to this one), and any instance may become the
/// one being upgraded, so it is on everywhere. The setting only takes effect
/// at server start, and this runs before the supervisor spawns postgres —
/// so every existing cluster converges on its next rolling restart.
pub fn ensure_wal_level_logical(pgdata: &Path) -> std::io::Result<bool> {
    let path = pgdata.join("postgresql.conf");
    let conf = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        // No data directory yet: initdb writes the current block itself.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !conf.contains("wal_level = replica") {
        return Ok(false);
    }
    std::fs::write(
        &path,
        conf.replace("wal_level = replica", "wal_level = logical"),
    )?;
    Ok(true)
}

/// Compare PGDATA's major (PG_VERSION file) with the running binary's.
/// Some((data, binary)) on a definite mismatch; None when they match or
/// either side is unreadable (postgres is the final authority).
pub fn major_version_conflict(
    pgdata_version: &str,
    binary_version_output: &str,
) -> Option<(String, String)> {
    let data = pgdata_version.trim().split('.').next()?.to_string();
    let binary = binary_version_output
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?
        .split('.')
        .next()?
        .to_string();
    if data.is_empty() || binary.is_empty() || data == binary {
        None
    } else {
        Some((data, binary))
    }
}

/// Refuse to start an existing PGDATA under a binary from a different major:
/// postgres would refuse too, but deep in startup with a muddier error. The
/// clear remedy — revert the image and use the upgrade flow, which never
/// reinterprets an existing data directory — matters more than the check.
async fn check_major_version(pgdata: &Path) -> anyhow::Result<()> {
    let Ok(data_version) = std::fs::read_to_string(pgdata.join("PG_VERSION")) else {
        return Ok(());
    };
    let Ok(out) = Command::new("postgres").arg("--version").output().await else {
        return Ok(());
    };
    let binary_version = String::from_utf8_lossy(&out.stdout).to_string();
    if let Some((data, binary)) = major_version_conflict(&data_version, &binary_version) {
        bail!(
            "PGDATA was created by PostgreSQL {data} but this image ships PostgreSQL {binary}; \
             revert the cluster image to a PostgreSQL {data} build — changing major version \
             is what the upgrade operation is for"
        );
    }
    Ok(())
}

/// Whether this boot should convert the data directory in place, and from
/// which major.
///
/// Some only when the marker names a major, PGDATA exists, and its version
/// still matches that major — so a pod restarting after a successful
/// conversion simply starts normally instead of trying to upgrade again.
pub fn in_place_pending(pgdata: &Path, cfg: &Config) -> Option<u32> {
    let want = cfg.in_place_major()?;
    let on_disk: u32 = std::fs::read_to_string(pgdata.join("PG_VERSION"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (on_disk == want).then_some(want)
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
    hba_rules: &[pglord_core::hba::HbaRule],
) -> anyhow::Result<()> {
    // Only the primary restores. Standbys of a restored cluster stream from it
    // with an ordinary basebackup, exactly like any other cluster's standbys.
    if cfg.role == Role::Primary && restore_pending(&cfg.pgdata, restore) {
        let restore = restore.expect("restore_pending implies Some");
        return bootstrap_restore(cfg, restore).await;
    }
    if is_bootstrapped(&cfg.pgdata) {
        if let Some(old_major) = in_place_pending(&cfg.pgdata, cfg) {
            return match cfg.role {
                // The primary converts its directory. Standbys cannot be
                // pg_upgraded at all, so they discard their data and clone the
                // converted primary through the ordinary basebackup path.
                Role::Primary => crate::pgupgrade::run(cfg, old_major, hba_rules).await,
                _ => {
                    tracing::warn!(
                        old_major,
                        "in-place upgrade: discarding standby data to re-clone from the primary"
                    );
                    std::fs::remove_dir_all(&cfg.pgdata).context("wipe standby PGDATA")?;
                    std::fs::create_dir_all(&cfg.pgdata).context("recreate PGDATA")?;
                    bootstrap_standby(cfg).await
                }
            };
        }
        check_major_version(&cfg.pgdata).await?;
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
        Role::Primary => bootstrap_primary(cfg, hba_rules).await,
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
    // connect_timeout matters after a primary pod restart: a walreceiver
    // retrying the old pod IP would otherwise hang in TCP connect for minutes
    // before re-resolving DNS — under synchronous replication that is minutes
    // of blocked writes instead of seconds.
    format!(
        "primary_conninfo = 'host={} port={} user={user} password={password} \
         application_name={application_name} connect_timeout=5'\n",
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

async fn bootstrap_primary(
    cfg: &Config,
    hba_rules: &[pglord_core::hba::HbaRule],
) -> anyhow::Result<()> {
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
    // The agent owns the whole file: a pure render of the spec, replacing
    // whatever initdb generated (same effective prefix — see hba::render).
    std::fs::write(
        cfg.pgdata.join("pg_hba.conf"),
        pglord_core::hba::render(hba_rules, &cfg.repl_user, scram),
    )
    .context("write pg_hba.conf")?;
    // A major-upgrade target inherits its data from the cluster being
    // upgraded, where initialisation already happened — same reasoning as a
    // restored cluster. Running the scripts here as well would collide with
    // the schema copied from the source and stall the migration.
    if !cfg.upgrade_source.is_empty() {
        crate::initdb::mark_initialised(&cfg.pgdata)
            .context("mark upgrade target as initialised")?;
    }
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
            // -R writes primary_conninfo without application_name; rewrite it
            // now so the standby is a nameable sync peer from its first start.
            let me = std::env::var("HOSTNAME").unwrap_or_default();
            refresh_conninfo(&cfg.pgdata, &primary_conninfo(cfg, &me))
                .context("set application_name in primary_conninfo")?;
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

pub(crate) fn append(path: &Path, content: &str) -> anyhow::Result<()> {
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
    fn major_versions_compared_from_pgdata_and_binary() {
        // PG_VERSION holds e.g. "16\n"; `postgres --version` prints
        // "postgres (PostgreSQL) 16.4 (Debian ...)".
        assert_eq!(
            major_version_conflict("16\n", "postgres (PostgreSQL) 16.4"),
            None
        );
        assert_eq!(
            major_version_conflict("15\n", "postgres (PostgreSQL) 16.4 (Debian 16.4-1)"),
            Some(("15".into(), "16".into()))
        );
        // Unparseable input must not block startup: postgres itself is the
        // final authority and refuses a real mismatch anyway.
        assert_eq!(major_version_conflict("16\n", "gibberish"), None);
        assert_eq!(
            major_version_conflict("", "postgres (PostgreSQL) 16.4"),
            None
        );
    }

    #[test]
    fn conf_extra_contains_replication_settings() {
        let conf = postgresql_conf_extra(5433);
        assert!(conf.contains("port = 5433"));
        // logical, not replica: any instance may become the source of a
        // blue-green major upgrade, and the level is only settable at start.
        assert!(conf.contains("wal_level = logical"));
        assert!(conf.contains("max_wal_senders = 10"));
        assert!(conf.contains("hot_standby = on"));
        assert!(conf.contains("listen_addresses = '*'"));
        assert!(conf.contains("wal_log_hints = on"));
        // The include must come before the managed settings so user parameters
        // cannot override them.
        let include_at = conf.find("include_if_exists").expect("include line");
        let wal_level_at = conf.find("wal_level").expect("wal_level line");
        assert!(include_at < wal_level_at);
        // The sync include follows the same rule as params/archive: included
        // before the managed settings so it can never override HA settings.
        assert!(conf.contains("include_if_exists = 'postgresql.pglord-sync.conf'"));
        let sync_at = conf
            .find("postgresql.pglord-sync.conf")
            .expect("sync include present");
        let managed_at = conf.find("listen_addresses").expect("managed settings");
        assert!(sync_at < managed_at);
        // The hot-standby floor include must come after the user parameter
        // include (last assignment wins, so the clamp overrides the spec).
        assert!(conf.contains("include_if_exists = 'postgresql.pglord-standby.conf'"));
        let params_at = conf
            .find("postgresql.pglord.conf")
            .expect("params include present");
        let standby_at = conf
            .find("postgresql.pglord-standby.conf")
            .expect("standby include present");
        assert!(params_at < standby_at);
        assert!(standby_at < managed_at);
        // The tls include is managed (users cannot set ssl* at all — they are
        // reserved), placed with the other managed includes before the fixed
        // settings.
        assert!(conf.contains("include_if_exists = 'postgresql.pglord-tls.conf'"));
        let tls_at = conf
            .find("postgresql.pglord-tls.conf")
            .expect("tls include present");
        assert!(tls_at < managed_at);
    }

    #[test]
    fn bootstrap_uses_the_core_hba_render() {
        // The file is a pure function of the spec; bootstrap must not add or
        // append anything of its own on top of the core render.
        let out = pglord_core::hba::render(&[], "replicator", true);
        assert!(out.contains("host replication replicator 0.0.0.0/0 scram-sha-256"));
        assert!(out.ends_with("host all all 0.0.0.0/0 scram-sha-256\n"));
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
        // A hung connect to a stale pod IP must fail fast so the walreceiver
        // re-resolves DNS; under sync replication writes block until it does.
        assert!(conninfo.contains("connect_timeout=5"));
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

    #[test]
    fn in_place_is_pending_only_for_an_older_pgdata_with_the_marker() {
        let dir = std::env::temp_dir().join("pglord-agent-test-inplace");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cfg_with = |v: &str| -> Config {
            let mut c: Config =
                serde_json::from_value(serde_json::json!({ "role": "primary" })).expect("config");
            c.upgrade_in_place = v.to_string();
            c.pgdata = dir.clone();
            c
        };

        // No data directory yet: a fresh instance initdbs, it does not upgrade.
        assert_eq!(in_place_pending(&dir, &cfg_with("16")), None);

        std::fs::write(dir.join("PG_VERSION"), "16\n").expect("write");
        assert_eq!(in_place_pending(&dir, &cfg_with("16")), Some(16));
        // No marker: the ordinary major-mismatch refusal must still apply.
        assert_eq!(in_place_pending(&dir, &cfg_with("")), None);
        // Already converted (a pod restart after success): nothing to do, or
        // the instance would try to upgrade its upgraded directory.
        std::fs::write(dir.join("PG_VERSION"), "17\n").expect("write");
        assert_eq!(in_place_pending(&dir, &cfg_with("16")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_wal_level_logical_rewrites_old_managed_blocks() {
        let dir = std::env::temp_dir().join("pglord-agent-test-wal-level");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        // No data directory yet: nothing to rewrite, and no error either.
        assert!(!ensure_wal_level_logical(&dir).expect("absent conf"));

        let conf = dir.join("postgresql.conf");
        std::fs::write(&conf, "listen_addresses = '*'\nwal_level = replica\n").expect("write");
        assert!(ensure_wal_level_logical(&dir).expect("rewrite"));
        let after = std::fs::read_to_string(&conf).expect("read");
        assert!(after.contains("wal_level = logical"));
        assert!(!after.contains("wal_level = replica"));
        assert!(after.contains("listen_addresses = '*'"));

        // Idempotent: a second pass must not rewrite the file again.
        assert!(!ensure_wal_level_logical(&dir).expect("noop"));
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
