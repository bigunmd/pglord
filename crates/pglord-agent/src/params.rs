//! User parameters applied to the local instance.
//!
//! The rendered settings live in their own file which postgresql.conf includes;
//! pglord's own managed settings are appended *after* the include so that no
//! user parameter can override the ones HA depends on.

use std::path::Path;

use tokio::process::Command;

pub const PARAMS_FILE: &str = "postgresql.pglord.conf";
/// Archiving settings, written separately so enabling backups does not touch
/// the user's parameter file (and so it can be cleared independently).
pub const ARCHIVE_FILE: &str = "postgresql.pglord-archive.conf";
/// Synchronous-replication settings, one file per instance: each instance's
/// peer list excludes itself, so a promoted standby already carries the right
/// value (see pglord_core::sync::render).
pub const SYNC_FILE: &str = "postgresql.pglord-sync.conf";
/// Hot-standby GUC floor (see pglord_core::standby::render); included after
/// the user parameter file so the clamp always wins.
pub const STANDBY_FILE: &str = "postgresql.pglord-standby.conf";
/// TLS settings (see pglord_core::tls::conf): ssl on/off and the cert paths
/// under the mounted Secret.
pub const TLS_FILE: &str = "postgresql.pglord-tls.conf";
/// Digest of the mounted cert files, kept so a cert-manager renewal (kubelet
/// swapping the projected files) is noticed and triggers exactly one reload.
const TLS_HASH_FILE: &str = "postgresql.pglord-tls.hash";

/// Rendered archiving settings: on with a pgBackRest archive_command when
/// backups are configured, explicitly off when they are not.
pub fn archive_conf(cluster: &str, enabled: bool) -> String {
    if enabled {
        format!(
            "# Managed by pglord: WAL archiving for backups.\n\
             archive_mode = on\n\
             archive_command = '{}'\n",
            pglord_core::backup::archive_command(cluster)
        )
    } else {
        // Without a repository, archiving would fill the disk with WAL that can
        // never be shipped anywhere.
        "# Managed by pglord: no backup repository configured.\narchive_mode = off\n".to_string()
    }
}

/// Write `rendered` to `path` unless it already has exactly that content;
/// returns whether anything was written. Content comparison is what keeps the
/// agent from signalling postgres on every poll.
fn write_file_if_changed(path: &Path, rendered: &str) -> std::io::Result<bool> {
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == rendered
    {
        return Ok(false);
    }
    std::fs::write(path, rendered)?;
    Ok(true)
}

/// Write the archiving file when it differs; returns whether it changed.
pub fn write_archive_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    write_file_if_changed(&pgdata.join(ARCHIVE_FILE), rendered)
}

/// Write the sync-replication file when it differs; returns whether it changed.
pub fn write_sync_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    write_file_if_changed(&pgdata.join(SYNC_FILE), rendered)
}

/// Write the hot-standby floor file when it differs; returns whether it changed.
pub fn write_standby_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    write_file_if_changed(&pgdata.join(STANDBY_FILE), rendered)
}

/// Write the rendered parameters, returning whether the content changed.
pub fn write_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    write_file_if_changed(&pgdata.join(PARAMS_FILE), rendered)
}

/// Write the TLS settings file when it differs; returns whether it changed.
pub fn write_tls_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    write_file_if_changed(&pgdata.join(TLS_FILE), rendered)
}

/// Track the mounted cert files' contents; returns whether they changed since
/// the last pass. Postgres re-reads its certificates on SIGHUP, so a `true`
/// here rides the converge pass's single reload and a renewed certificate is
/// served within one poll interval, with no restart.
///
/// `DefaultHasher` is change detection, not security: the worst a collision
/// costs is a missed reload until the next content change, and the worst an
/// unstable hash across agent upgrades costs is one spurious reload.
pub fn write_tls_rotation_marker(pgdata: &Path, cert_dir: &Path) -> std::io::Result<bool> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    for key in [
        pglord_core::tls::TLS_CERT_KEY,
        pglord_core::tls::TLS_KEY_KEY,
        pglord_core::tls::TLS_CA_KEY,
    ] {
        match std::fs::read(cert_dir.join(key)) {
            Ok(bytes) => {
                key.hash(&mut h);
                bytes.hash(&mut h);
            }
            Err(_) => "absent".hash(&mut h),
        }
    }
    write_file_if_changed(&pgdata.join(TLS_HASH_FILE), &format!("{:x}\n", h.finish()))
}

/// Write the rendered pg_hba.conf when it differs; returns whether it changed.
/// Unlike the parameter files this is not an include — the agent owns the
/// whole file (see pglord_core::hba::render).
pub fn write_hba_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    write_file_if_changed(&pgdata.join("pg_hba.conf"), rendered)
}

/// Ask postgres to re-read its configuration. Reload-safe settings take effect
/// immediately; the rest show up as pending_restart.
pub async fn reload(pgdata: &Path) -> anyhow::Result<()> {
    let status = Command::new("pg_ctl")
        .arg("reload")
        .arg("-D")
        .arg(pgdata)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("pg_ctl reload failed with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pglord-params-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn writes_once_and_detects_no_change() {
        let dir = tempdir("write");
        assert!(write_if_changed(&dir, "work_mem = 16MB\n").expect("write"));
        // Same content must not report a change: otherwise every poll would
        // reload postgres.
        assert!(!write_if_changed(&dir, "work_mem = 16MB\n").expect("write"));
        assert!(write_if_changed(&dir, "work_mem = 32MB\n").expect("write"));
        assert_eq!(
            std::fs::read_to_string(dir.join(PARAMS_FILE)).expect("read"),
            "work_mem = 32MB\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn archiving_is_off_without_a_repository() {
        let off = archive_conf("demo", false);
        assert!(off.contains("archive_mode = off"));
        assert!(!off.contains("archive_command"));

        let on = archive_conf("demo", true);
        assert!(on.contains("archive_mode = on"));
        assert!(on.contains("pgbackrest --stanza=pglord-demo archive-push %p"));
    }

    #[test]
    fn clearing_parameters_writes_an_empty_file() {
        let dir = tempdir("clear");
        write_if_changed(&dir, "work_mem = 16MB\n").expect("write");
        assert!(write_if_changed(&dir, "").expect("write"));
        assert_eq!(
            std::fs::read_to_string(dir.join(PARAMS_FILE)).expect("read"),
            ""
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_write_is_idempotent_and_targets_the_sync_file() {
        let dir = tempdir("sync");
        let on = pglord_core::sync::render("demo", 2, "demo-0", true);
        assert!(write_sync_if_changed(&dir, &on).expect("write"));
        // Unchanged content must not report a change: a report triggers a
        // reload, and this runs on every 5s poll.
        assert!(!write_sync_if_changed(&dir, &on).expect("write"));
        assert_eq!(
            std::fs::read_to_string(dir.join(SYNC_FILE)).expect("read"),
            on
        );
        let off = pglord_core::sync::render("demo", 2, "demo-0", false);
        assert!(write_sync_if_changed(&dir, &off).expect("write"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn standby_write_is_idempotent_and_targets_the_standby_file() {
        let dir = tempdir("standby");
        let live = pglord_core::standby::parse_controldata(
            "max_connections setting: 150\nmax_worker_processes setting: 8\n\
             max_prepared_xacts setting: 0\nmax_locks_per_xact setting: 64\n",
        )
        .expect("parse");
        let clamped = pglord_core::standby::render(&Default::default(), Some(&live));
        assert!(write_standby_if_changed(&dir, &clamped).expect("write"));
        // Unchanged content must not report a change: a report triggers a
        // reload, and this runs on every 5s poll.
        assert!(!write_standby_if_changed(&dir, &clamped).expect("write"));
        assert_eq!(
            std::fs::read_to_string(dir.join(STANDBY_FILE)).expect("read"),
            clamped
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_write_is_idempotent_and_targets_the_tls_file() {
        let dir = tempdir("tls");
        let on = pglord_core::tls::conf(true, false);
        assert!(write_tls_if_changed(&dir, &on).expect("write"));
        // Unchanged content must not report a change: a report triggers a
        // reload, and this runs on every 5s poll.
        assert!(!write_tls_if_changed(&dir, &on).expect("write"));
        assert_eq!(std::fs::read_to_string(dir.join(TLS_FILE)).expect("read"), on);
        let off = pglord_core::tls::conf(false, false);
        assert!(write_tls_if_changed(&dir, &off).expect("write"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_marker_fires_once_per_cert_change() {
        let pgdata = tempdir("tls-rotate-pgdata");
        let certs = tempdir("tls-rotate-certs");
        std::fs::write(certs.join("tls.crt"), "CERT-A").expect("write");
        std::fs::write(certs.join("tls.key"), "KEY-A").expect("write");

        // First sighting of these certs registers as a change...
        assert!(write_tls_rotation_marker(&pgdata, &certs).expect("marker"));
        // ...and stays quiet on every following poll.
        assert!(!write_tls_rotation_marker(&pgdata, &certs).expect("marker"));

        // A renewal (kubelet swapping the projected files) fires exactly once.
        std::fs::write(certs.join("tls.crt"), "CERT-B").expect("write");
        assert!(write_tls_rotation_marker(&pgdata, &certs).expect("marker"));
        assert!(!write_tls_rotation_marker(&pgdata, &certs).expect("marker"));

        // A CA appearing later is a change too (the conf gains ssl_ca_file).
        std::fs::write(certs.join("ca.crt"), "CA").expect("write");
        assert!(write_tls_rotation_marker(&pgdata, &certs).expect("marker"));
        assert!(!write_tls_rotation_marker(&pgdata, &certs).expect("marker"));

        let _ = std::fs::remove_dir_all(&pgdata);
        let _ = std::fs::remove_dir_all(&certs);
    }

    #[test]
    fn hba_write_is_idempotent_and_targets_pg_hba_conf() {
        let dir = tempdir("hba");
        let rendered = pglord_core::hba::render(&[], "replicator", true);
        assert!(write_hba_if_changed(&dir, &rendered).expect("write"));
        // Unchanged content must not report a change: a report triggers a
        // reload, and this runs on every 5s poll.
        assert!(!write_hba_if_changed(&dir, &rendered).expect("write"));
        assert_eq!(
            std::fs::read_to_string(dir.join("pg_hba.conf")).expect("read"),
            rendered
        );
        let with_rule = pglord_core::hba::render(
            &[pglord_core::hba::HbaRule {
                conn: pglord_core::hba::HbaConn::Host,
                database: "all".into(),
                user: "intruder".into(),
                address: "0.0.0.0/0".into(),
                method: pglord_core::hba::HbaMethod::Reject,
            }],
            "replicator",
            true,
        );
        assert!(write_hba_if_changed(&dir, &with_rule).expect("write"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
