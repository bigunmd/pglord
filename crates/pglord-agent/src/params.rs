//! User parameters applied to the local instance.
//!
//! The rendered settings live in their own file which postgresql.conf includes;
//! pglord's own managed settings are appended *after* the include so that no
//! user parameter can override the ones HA depends on.

use std::path::Path;

use tokio::process::Command;

pub const PARAMS_FILE: &str = "postgresql.pglord.conf";

/// Write the rendered parameters, returning whether the content changed.
/// Comparing content (rather than reloading unconditionally) keeps the agent
/// from signalling postgres on every poll.
pub fn write_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    let path = pgdata.join(PARAMS_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path)
        && existing == rendered
    {
        return Ok(false);
    }
    std::fs::write(&path, rendered)?;
    Ok(true)
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
}
