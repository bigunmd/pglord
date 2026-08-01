//! Running a cluster's initialisation scripts.
//!
//! Once, on the primary, after postgres accepts connections and the roles are
//! in place — and never again. The marker lives in PGDATA, so it is copied to
//! standbys by pg_basebackup: a standby that is later promoted must not decide
//! it is time to initialise a database that has been serving for months.

use std::path::Path;

use anyhow::Context;
use pglord_core::init::{self, INIT_MARKER};
use tokio::process::Command;

use crate::config::Config;

/// Whether initialisation still has to run for this data directory.
pub fn init_pending(pgdata: &Path) -> bool {
    !pgdata.join(INIT_MARKER).exists()
}

/// Record that the initialisation phase is over.
///
/// Written even when no scripts were configured. Adding scripts to a cluster
/// that is already running must not run them against existing data — init
/// scripts describe how a database is created, not how it is migrated.
pub fn mark_initialised(pgdata: &Path) -> std::io::Result<()> {
    std::fs::write(pgdata.join(INIT_MARKER), "")
}

/// The `.sql` files in a directory, in the order they should run.
///
/// Sorted by file name so `01-schema.sql` reliably precedes `02-roles.sql`;
/// directory order is not sorted on any filesystem worth relying on.
pub fn scripts_in(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(init::is_script))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Run every configured script, in order, against the local instance.
///
/// A failing script aborts the whole run: a database left half-initialised
/// should be loud about it rather than start serving an incomplete schema.
pub async fn run_scripts(
    cfg: &Config,
    spec: &pglord_core::spec::ClusterSpec,
) -> anyhow::Result<()> {
    let dirs = init::script_dirs(&spec.init);
    if dirs.is_empty() {
        return Ok(());
    }
    let vars = psql_variables(spec);
    for dir in dirs {
        for script in scripts_in(Path::new(dir)) {
            tracing::info!(script = %script.display(), "running init script");
            let mut cmd = Command::new("psql");
            cmd.args(["-U", &cfg.superuser])
                .arg("-h")
                .arg(&cfg.socket_dir)
                .arg("-p")
                .arg(cfg.port.to_string())
                .args(["-d", "postgres"])
                // Stop at the first failing statement instead of running the
                // rest of the file against a database in an unexpected state.
                .args(["-v", "ON_ERROR_STOP=1"])
                .arg("--no-psqlrc");
            for (name, value) in &vars {
                cmd.arg("-v").arg(format!("{name}={value}"));
            }
            let out = cmd
                .arg("-f")
                .arg(&script)
                .output()
                .await
                .with_context(|| format!("spawn psql for {}", script.display()))?;
            if !out.status.success() {
                // psql's own message names the statement and line.
                anyhow::bail!(
                    "init script {} failed: {}",
                    script.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
    }
    Ok(())
}

/// The cluster's custom environment variables, as psql variables.
///
/// This is what lets an init script use a value that came from a Secret:
/// `CREATE ROLE app LOGIN PASSWORD :'APP_PASSWORD'`. Values are read from the
/// process environment, which is where the reconciler put them.
fn psql_variables(spec: &pglord_core::spec::ClusterSpec) -> Vec<(String, String)> {
    spec.env
        .iter()
        .filter_map(|e| {
            std::env::var(&e.name)
                .ok()
                .map(|value| (e.name.clone(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pglord-initdb-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn initialisation_runs_once_per_data_directory() {
        let dir = tempdir("marker");
        assert!(init_pending(&dir));
        mark_initialised(&dir).expect("mark");
        assert!(!init_pending(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scripts_run_in_file_name_order_and_ignore_everything_else() {
        let dir = tempdir("scripts");
        for name in [
            "02-roles.sql",
            "01-schema.sql",
            "10-seed.sql",
            "notes.txt",
            "seed.sh",
            "..data",
        ] {
            std::fs::write(dir.join(name), "").expect("write");
        }
        let names: Vec<String> = scripts_in(&dir)
            .iter()
            .map(|p| p.file_name().expect("name").to_string_lossy().to_string())
            .collect();
        // Lexical order, which is why the convention is zero-padded prefixes.
        assert_eq!(names, vec!["01-schema.sql", "02-roles.sql", "10-seed.sql"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        // The volume is only mounted when scripts are configured; asking for
        // scripts from a directory that is not there must not panic.
        assert!(scripts_in(Path::new("/nonexistent/pglord/initdb.d")).is_empty());
    }

    #[test]
    fn only_declared_variables_reach_psql() {
        // Reading the whole environment would hand every script the agent's own
        // configuration, including the superuser password.
        let spec: pglord_core::spec::ClusterSpec = serde_json::from_value(serde_json::json!({
            "name": "demo",
            "env": [
                { "name": "PGLORD_TEST_DECLARED", "value": "ignored-here" },
                { "name": "PGLORD_TEST_ABSENT", "value": "also-ignored" }
            ]
        }))
        .expect("spec");
        // SAFETY: single-threaded test process, and the name is test-specific.
        unsafe {
            std::env::set_var("PGLORD_TEST_DECLARED", "from-secret");
            std::env::remove_var("PGLORD_TEST_ABSENT");
        }
        let vars = psql_variables(&spec);
        assert_eq!(
            vars,
            vec![(
                "PGLORD_TEST_DECLARED".to_string(),
                "from-secret".to_string()
            )],
            "the value comes from the container environment, not the spec"
        );
        unsafe { std::env::remove_var("PGLORD_TEST_DECLARED") };
    }
}
