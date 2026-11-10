//! In-place major upgrades: converting this instance's data directory with
//! `pg_upgrade --link`.
//!
//! Argument construction is pure and unit-tested, because the command it
//! builds rewrites a live database and the only way back is a backup.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePlan {
    /// Freshly initdb'd directory pg_upgrade converts into. A sibling of
    /// PGDATA, because --link's hard links cannot cross filesystems.
    pub new_datadir: PathBuf,
    pub old_bindir: String,
    pub new_bindir: String,
    /// Non-destructive compatibility check.
    pub check_args: Vec<String>,
    /// The real conversion.
    pub run_args: Vec<String>,
}

pub fn plan(old_major: u32, new_bindir: &str, pgdata: &Path) -> UpgradePlan {
    let new_datadir = pgdata.with_file_name(format!(
        "{}.new",
        pgdata.file_name().unwrap_or_default().to_string_lossy()
    ));
    let old_bindir = format!("{}/bin", pglord_core::render::old_lib_path(old_major));
    let base = vec![
        format!("--old-datadir={}", pgdata.display()),
        format!("--new-datadir={}", new_datadir.display()),
        format!("--old-bindir={old_bindir}"),
        format!("--new-bindir={new_bindir}"),
        "--link".to_string(),
    ];
    let mut check_args = base.clone();
    check_args.push("--check".to_string());
    UpgradePlan {
        new_datadir,
        old_bindir,
        new_bindir: new_bindir.to_string(),
        check_args,
        run_args: base,
    }
}

/// The bindir of the major this image ships, discovered from `pg_config` so it
/// never has to be configured per image.
fn new_bindir() -> String {
    std::process::Command::new("pg_config")
        .arg("--bindir")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/bin".to_string())
}

const READY_MARKER: &str = "pglord.upgrade-ready";

fn sibling(pgdata: &Path, suffix: &str) -> PathBuf {
    pgdata.with_file_name(format!(
        "{}.{suffix}",
        pgdata.file_name().unwrap_or_default().to_string_lossy()
    ))
}

/// Resume only a fully converted and configured target. Ambiguous artifacts
/// require recovery, never initdb or another destructive conversion attempt.
pub fn recover_handoff(pgdata: &Path) -> anyhow::Result<bool> {
    let new = sibling(pgdata, "new");
    let old = sibling(pgdata, "old");
    if !new.exists() && !old.exists() {
        return Ok(false);
    }
    if new.exists() {
        if !new.join(READY_MARKER).is_file() || !new.join("PG_VERSION").is_file() {
            bail!("incomplete in-place upgrade; refusing initialization or automatic retry");
        }
        if pgdata.exists() {
            if old.exists() {
                bail!("ambiguous in-place upgrade directories; manual recovery required");
            }
            std::fs::rename(pgdata, &old).context("preserve pre-upgrade directory")?;
            sync_parent(pgdata)?;
        }
        std::fs::rename(&new, pgdata).context("promote converted directory")?;
        sync_parent(pgdata)?;
    }
    if !pgdata.join(READY_MARKER).is_file() || !pgdata.join("PG_VERSION").is_file() {
        bail!("missing completed upgrade directory; refusing initialization");
    }
    if old.exists() {
        // The checked canonical directory owns the converted hard links now.
        std::fs::remove_dir_all(&old).context("remove preserved old directory")?;
        sync_parent(pgdata)?;
    }
    Ok(true)
}

fn preserve_restore_marker(source: &Path, target: &Path) -> anyhow::Result<()> {
    let marker = crate::bootstrap::RESTORE_MARKER;
    if source.join(marker).is_file() {
        std::fs::copy(source.join(marker), target.join(marker))?;
        std::fs::File::open(target.join(marker))?.sync_all()?;
    }
    Ok(())
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    std::fs::File::open(path.parent().context("PGDATA has no parent")?)?.sync_all()?;
    Ok(())
}

/// Convert PGDATA to this binary's major.
///
/// Interrupted conversion is fail-closed: --link may have modified old data,
/// so an incomplete target must not be discarded and retried automatically.
pub async fn run(
    cfg: &Config,
    old_major: u32,
    hba_rules: &[pglord_core::hba::HbaRule],
) -> anyhow::Result<()> {
    let p = plan(old_major, &new_bindir(), &cfg.pgdata);
    tracing::warn!(
        old_major,
        new_bindir = %p.new_bindir,
        "in-place upgrade: converting the data directory"
    );

    if recover_handoff(&cfg.pgdata)? {
        return Ok(());
    }
    let status = tokio::process::Command::new(format!("{}/initdb", p.new_bindir))
        .arg("-D")
        .arg(&p.new_datadir)
        .args(["-U", &cfg.superuser, "-A", "trust"])
        .status()
        .await
        .context("spawn initdb for the upgrade target")?;
    if !status.success() {
        bail!("initdb for the upgrade target failed with {status}");
    }

    for (label, args) in [("check", &p.check_args), ("run", &p.run_args)] {
        let out = tokio::process::Command::new(format!("{}/pg_upgrade", p.new_bindir))
            .args(args)
            // pg_upgrade writes its logs into the working directory, which
            // must be writable and must not be inside either data directory.
            .current_dir(cfg.pgdata.parent().unwrap_or(&cfg.pgdata))
            .output()
            .await
            .with_context(|| format!("spawn pg_upgrade ({label})"))?;
        if !out.status.success() {
            bail!(
                "pg_upgrade {label} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        tracing::info!(label, "pg_upgrade stage complete");
    }

    // The converted directory came from a bare initdb and carries none of
    // pglord's configuration.
    crate::bootstrap::append(
        &p.new_datadir.join("postgresql.conf"),
        &crate::bootstrap::postgresql_conf_extra(cfg.port),
    )?;
    std::fs::write(
        p.new_datadir.join("pg_hba.conf"),
        pglord_core::hba::render(
            hba_rules,
            &cfg.repl_user,
            !cfg.superuser_password.is_empty(),
        ),
    )
    .context("write pg_hba.conf")?;
    preserve_restore_marker(&cfg.pgdata, &p.new_datadir)?;
    for name in ["postgresql.conf", "pg_hba.conf"] {
        std::fs::File::open(p.new_datadir.join(name))?.sync_all()?;
    }
    std::fs::File::create(p.new_datadir.join(READY_MARKER))?.sync_all()?;
    std::fs::File::open(&p.new_datadir)?.sync_all()?;
    recover_handoff(&cfg.pgdata)?;
    tracing::warn!(old_major, "in-place upgrade complete");
    Ok(())
}

/// Rebuild optimizer statistics, which pg_upgrade does not carry over.
///
/// Best-effort: without it the cluster is correct but slow, which reads to a
/// user as a failed upgrade, yet failing the boot over statistics would be
/// worse than being slow.
pub async fn analyze(cfg: &Config) {
    let out = tokio::process::Command::new("vacuumdb")
        .args([
            "-U",
            &cfg.superuser,
            "-h",
            &cfg.socket_dir.to_string_lossy(),
            "-p",
            &cfg.port.to_string(),
            "--all",
            "--analyze-in-stages",
        ])
        .env("PGPASSWORD", &cfg.superuser_password)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            tracing::info!("post-upgrade statistics rebuilt");
        }
        Ok(o) => tracing::warn!(
            stderr = %String::from_utf8_lossy(&o.stderr).trim(),
            "post-upgrade vacuumdb failed; the cluster is correct but may be slow"
        ),
        Err(err) => tracing::warn!(%err, "post-upgrade vacuumdb could not run"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_recovers_each_rename_boundary_and_refuses_partial_conversion() {
        let root = std::env::temp_dir().join(format!(
            "pglord-handoff-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&root).expect("unique test directory");
        for boundary in 0..3 {
            let data = root.join(format!("pgdata{boundary}"));
            let new = sibling(&data, "new");
            let old = sibling(&data, "old");
            let source = if boundary == 0 { &data } else { &old };
            std::fs::create_dir(source).expect("source");
            std::fs::write(source.join("PG_VERSION"), "16").expect("old version");
            let target = if boundary == 2 { &data } else { &new };
            std::fs::create_dir(target).expect("target");
            std::fs::write(target.join("PG_VERSION"), "17").expect("new version");
            std::fs::write(target.join("rows"), "user data").expect("data");
            std::fs::write(source.join(crate::bootstrap::RESTORE_MARKER), "")
                .expect("restore marker");
            preserve_restore_marker(source, target).expect("preserve restore completion");
            std::fs::write(target.join(READY_MARKER), "").expect("ready");
            assert!(recover_handoff(&data).expect("recover"));
            assert_eq!(
                std::fs::read_to_string(data.join("rows")).expect("rows"),
                "user data"
            );
            assert!(data.join(crate::bootstrap::RESTORE_MARKER).is_file());
            assert!(!old.exists());
            assert!(!new.exists());
            assert!(!recover_handoff(&data).expect("idempotent"));
        }
        let data = root.join("incomplete");
        std::fs::create_dir(sibling(&data, "new")).expect("partial target");
        assert!(recover_handoff(&data).is_err());
        assert!(!data.exists(), "must not initialize missing PGDATA");
        // Only the unique directory created by this test is removed.
        assert!(root.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(root).expect("cleanup test directory");
    }

    #[test]
    fn the_new_datadir_is_a_sibling_of_pgdata() {
        // --link makes hard links, which cannot cross filesystems: the new
        // directory has to live on the same volume as the old one.
        let p = plan(
            16,
            "/usr/lib/postgresql/17/bin",
            Path::new("/var/lib/postgresql/data/pgdata"),
        );
        assert_eq!(
            p.new_datadir,
            Path::new("/var/lib/postgresql/data/pgdata.new")
        );
        assert_eq!(
            p.new_datadir.parent(),
            Path::new("/var/lib/postgresql/data/pgdata").parent()
        );
    }

    #[test]
    fn old_bindir_points_at_the_mounted_tree() {
        let p = plan(16, "/usr/lib/postgresql/17/bin", Path::new("/d/pgdata"));
        assert_eq!(p.old_bindir, "/usr/lib/postgresql/16/bin");
        assert_eq!(p.new_bindir, "/usr/lib/postgresql/17/bin");
    }

    #[test]
    fn check_args_are_the_run_args_plus_check() {
        let p = plan(16, "/nb", Path::new("/d/pgdata"));
        assert!(p.check_args.contains(&"--check".to_string()));
        assert!(!p.run_args.contains(&"--check".to_string()));
        // The real run links; anything else defeats the point of the mode.
        assert!(p.run_args.contains(&"--link".to_string()));
        assert!(p.check_args.contains(&"--link".to_string()));
        for args in [&p.check_args, &p.run_args] {
            assert!(
                args.contains(&"--old-datadir=/d/pgdata".to_string()),
                "{args:?}"
            );
            assert!(
                args.contains(&"--new-datadir=/d/pgdata.new".to_string()),
                "{args:?}"
            );
            assert!(
                args.contains(&"--old-bindir=/usr/lib/postgresql/16/bin".to_string()),
                "{args:?}"
            );
            assert!(args.contains(&"--new-bindir=/nb".to_string()), "{args:?}");
        }
    }
}
