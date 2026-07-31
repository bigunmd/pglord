//! pgBackRest driving: write the repository config, own the stanza, take
//! backups, and report what the repository holds.
//!
//! Object-storage credentials come from the environment (mounted from the
//! user's Secret) and are passed to pgbackrest the same way, so they never
//! reach a config file or a log line.

use std::path::Path;

use pglord_core::backup::{self, BackupSpec};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupInfo {
    pub label: String,
    /// "full", "diff" or "incr".
    pub kind: String,
    /// Unix seconds.
    pub started: i64,
    pub finished: i64,
    /// Bytes written to the repository for this backup.
    pub size_bytes: i64,
    pub wal_start: Option<String>,
    pub wal_end: Option<String>,
}

/// Write pgbackrest.conf when it differs; returns whether it changed.
pub fn write_conf_if_changed(cfg: &Config, spec: &BackupSpec) -> std::io::Result<bool> {
    let rendered = backup::pgbackrest_conf(&cfg.cluster, &cfg.pgdata.display().to_string(), spec);
    let path = Path::new(backup::CONF_PATH);
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == rendered
    {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, rendered)?;
    Ok(true)
}

fn command(cfg: &Config, args: &[&str]) -> Command {
    let mut cmd = Command::new("pgbackrest");
    cmd.arg(format!("--stanza={}", backup::stanza_name(&cfg.cluster)))
        .args(args);
    // pgbackrest reads these from the environment; nothing is logged.
    if !cfg.s3_access_key.is_empty() {
        cmd.env("PGBACKREST_REPO1_S3_KEY", &cfg.s3_access_key);
        cmd.env("PGBACKREST_REPO1_S3_KEY_SECRET", &cfg.s3_secret_key);
    }
    cmd
}

/// Create the stanza if the repository does not have one yet. Idempotent:
/// pgbackrest treats an existing, matching stanza as success.
pub async fn ensure_stanza(cfg: &Config) -> anyhow::Result<()> {
    let status = command(cfg, &["stanza-create"]).status().await?;
    if !status.success() {
        // An existing stanza from a previous run is not an error worth failing
        // startup over; upgrade attempts to log and continue.
        tracing::warn!(%status, "stanza-create returned non-zero (may already exist)");
    }
    Ok(())
}

pub async fn take_backup(cfg: &Config, kind: &str) -> anyhow::Result<()> {
    let kind = match kind {
        "incr" | "diff" | "full" => kind,
        other => anyhow::bail!("unknown backup type '{other}'"),
    };
    let status = command(cfg, &["backup", &format!("--type={kind}")])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("pgbackrest backup failed with {status}");
    }
    Ok(())
}

pub async fn list_backups(cfg: &Config) -> anyhow::Result<Vec<BackupInfo>> {
    let out = command(cfg, &["info", "--output=json"]).output().await?;
    if !out.status.success() {
        anyhow::bail!("pgbackrest info failed with {}", out.status);
    }
    parse_info(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `pgbackrest info --output=json`. Kept separate from the process call
/// so the shape can be tested against a captured fixture.
pub fn parse_info(json: &str) -> anyhow::Result<Vec<BackupInfo>> {
    let stanzas: serde_json::Value = serde_json::from_str(json)?;
    let mut out = Vec::new();
    for stanza in stanzas.as_array().unwrap_or(&Vec::new()) {
        for b in stanza["backup"].as_array().unwrap_or(&Vec::new()) {
            out.push(BackupInfo {
                label: b["label"].as_str().unwrap_or_default().to_string(),
                kind: b["type"].as_str().unwrap_or("unknown").to_string(),
                started: b["timestamp"]["start"].as_i64().unwrap_or_default(),
                finished: b["timestamp"]["stop"].as_i64().unwrap_or_default(),
                size_bytes: b["info"]["repository"]["delta"]
                    .as_i64()
                    .or_else(|| b["info"]["repository"]["size"].as_i64())
                    .unwrap_or_default(),
                wal_start: b["archive"]["start"].as_str().map(str::to_string),
                wal_end: b["archive"]["stop"].as_str().map(str::to_string),
            });
        }
    }
    // Newest first: that is the order the UI shows and the order that matters
    // when picking a restore target later.
    out.sort_by(|a, b| b.started.cmp(&a.started));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO_JSON: &str = r#"[
      {
        "name": "pglord-demo",
        "backup": [
          {
            "label": "20260731-120000F",
            "type": "full",
            "timestamp": { "start": 1785500000, "stop": 1785500060 },
            "info": { "repository": { "delta": 1048576, "size": 2097152 } },
            "archive": { "start": "000000010000000000000002", "stop": "000000010000000000000003" }
          },
          {
            "label": "20260731-130000I",
            "type": "incr",
            "timestamp": { "start": 1785503600, "stop": 1785503620 },
            "info": { "repository": { "delta": 4096 } },
            "archive": { "start": "000000010000000000000004", "stop": "000000010000000000000004" }
          }
        ]
      }
    ]"#;

    #[test]
    fn info_parses_newest_first() {
        let backups = parse_info(INFO_JSON).expect("parse");
        assert_eq!(backups.len(), 2);
        assert_eq!(backups[0].label, "20260731-130000I");
        assert_eq!(backups[0].kind, "incr");
        assert_eq!(backups[0].size_bytes, 4096);
        assert_eq!(backups[1].kind, "full");
        assert_eq!(backups[1].size_bytes, 1048576);
        assert_eq!(
            backups[1].wal_start.as_deref(),
            Some("000000010000000000000002")
        );
    }

    #[test]
    fn empty_repository_is_not_an_error() {
        // A freshly created stanza reports a stanza with no backups; that is a
        // normal state, not a failure.
        let empty = r#"[{"name":"pglord-demo","backup":[]}]"#;
        assert_eq!(parse_info(empty).expect("parse").len(), 0);
        assert_eq!(parse_info("[]").expect("parse").len(), 0);
    }

    #[test]
    fn malformed_output_is_reported() {
        assert!(parse_info("not json").is_err());
    }
}
