//! Primary-record awareness. When the agent runs inside Kubernetes with the
//! instance ServiceAccount, it reads the spec ConfigMap's primary annotation
//! and follows it: a standby that sees itself recorded promotes; a running
//! primary that sees itself deposed fences (stops postgres, pg_rewind against
//! the new primary, prepares standby files, exits so the pod restarts clean).
//! Without Kubernetes access (docker e2e, local dev) the feature is inert.

use std::time::Duration;

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::Api;
use pglord_core::PRIMARY_ANNOTATION;
use pglord_core::render::pod_fqdn;
use tokio::process::Command;

use crate::config::{Config, Role};
use crate::status;

pub struct RecordReader {
    api: Api<ConfigMap>,
    cluster: String,
}

impl RecordReader {
    /// Available only in-cluster with PGLORD_AGENT_CLUSTER set.
    pub async fn try_new(cfg: &Config) -> Option<RecordReader> {
        if cfg.cluster.is_empty() {
            return None;
        }
        match kube::Client::try_default().await {
            Ok(client) => Some(RecordReader {
                api: Api::namespaced(client, &cfg.namespace),
                cluster: cfg.cluster.clone(),
            }),
            Err(err) => {
                tracing::info!(%err, "no kube access; primary-record following disabled");
                None
            }
        }
    }

    pub async fn recorded_primary(&self) -> Option<String> {
        let cm = self
            .api
            .get_opt(&format!("{}-spec", self.cluster))
            .await
            .ok()??;
        Some(
            cm.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(PRIMARY_ANNOTATION))
                .cloned()
                .unwrap_or_else(|| format!("{}-0", self.cluster)),
        )
    }

    /// The cluster spec as stored, when readable.
    pub async fn spec(&self) -> Option<pglord_core::spec::ClusterSpec> {
        let cm = self
            .api
            .get_opt(&format!("{}-spec", self.cluster))
            .await
            .ok()??;
        let raw = cm.data.as_ref()?.get("spec")?;
        serde_json::from_str(raw).ok()
    }
}

/// Whether postgres is replaying archived WAL to reach a restore target.
///
/// `recovery.signal` means archive recovery, which postgres finishes and exits
/// by itself; a standby waiting on a primary carries `standby.signal` instead.
/// Distinguishing the two is what keeps the promotion path from truncating a
/// restore that is still running.
pub fn archive_recovery_in_progress(pgdata: &std::path::Path) -> bool {
    pgdata.join("recovery.signal").exists()
}

/// Decide the bootstrap role from the record (pure; unit-tested).
pub fn role_from_record(recorded: &str, me: &str) -> Role {
    if recorded == me {
        Role::Primary
    } else {
        Role::Standby
    }
}

/// Resolve role + primary_host from the record when available; the caller
/// falls back to ordinal resolution otherwise.
pub async fn apply_record_to_config(
    cfg: &mut Config,
    reader: &RecordReader,
    me: &str,
) -> Option<Role> {
    let recorded = reader.recorded_primary().await?;
    let role = role_from_record(&recorded, me);
    if role == Role::Standby {
        cfg.primary_host = pod_fqdn(&recorded, &cfg.cluster, &cfg.namespace);
    }
    tracing::info!(%recorded, ?role, "role resolved from primary record");
    Some(role)
}

/// Follow the record forever. Never returns except by process exit (fence).
pub async fn follow(reader: RecordReader, cfg: std::sync::Arc<Config>, me: String) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let Some(recorded) = reader.recorded_primary().await else {
            continue;
        };
        let st = status::query_status(&cfg).await;
        if !st.ready {
            continue;
        }
        apply_parameters(&reader, &cfg).await;
        let i_am_recorded = recorded == me;
        match st.in_recovery {
            // A restoring instance is in recovery *and* recorded as primary,
            // but promoting it would end recovery early — losing WAL that had
            // not been replayed yet, or stopping short of the requested point
            // in time. Postgres removes recovery.signal and promotes itself
            // when it is genuinely finished; until then, leave it alone.
            Some(true) if i_am_recorded && archive_recovery_in_progress(&cfg.pgdata) => {
                tracing::info!("archive recovery still in progress; not promoting yet");
            }
            Some(true) if i_am_recorded => {
                tracing::warn!("record names me primary but I am in recovery: promoting");
                let _ = Command::new("pg_ctl")
                    .arg("promote")
                    .arg("-D")
                    .arg(&cfg.pgdata)
                    .status()
                    .await;
            }
            Some(false) if !i_am_recorded => {
                tracing::warn!(new_primary = %recorded, "deposed by primary record: fencing");
                fence(&cfg, &recorded).await;
            }
            _ => {}
        }
    }
}

/// Apply everything the spec drives on this instance: user parameters,
/// archiving, and the backup repository config. Reload-safe settings take
/// effect at once; the rest surface as pending_restart.
async fn apply_parameters(reader: &RecordReader, cfg: &Config) {
    let Some(spec) = reader.spec().await else {
        return;
    };
    let mut changed = false;

    let rendered = pglord_core::guc::render_parameters(&spec.parameters);
    match crate::params::write_if_changed(&cfg.pgdata, &rendered) {
        Ok(true) => changed = true,
        Ok(false) => {}
        Err(err) => tracing::warn!(%err, "failed to write parameter file"),
    }

    let archive = crate::params::archive_conf(&cfg.cluster, spec.backup.enabled);
    match crate::params::write_archive_if_changed(&cfg.pgdata, &archive) {
        Ok(true) => changed = true,
        Ok(false) => {}
        Err(err) => tracing::warn!(%err, "failed to write archive settings"),
    }

    if spec.backup.enabled {
        match crate::backup::write_conf_if_changed(cfg, &spec.backup) {
            Ok(true) => tracing::info!("backup repository configuration updated"),
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, "failed to write pgbackrest.conf"),
        }
    }

    if changed {
        tracing::info!("configuration changed, reloading postgres");
        if let Err(err) = crate::params::reload(&cfg.pgdata).await {
            tracing::warn!(%err, "reload failed");
        }
    }
}

/// Fence a deposed primary: stop postgres and exit. Nothing else — the
/// restarted agent resolves its role from the record (standby), finds
/// primary-shaped PGDATA, and runs pg_rewind at STARTUP where nothing can
/// race the process supervisor (see bootstrap::ensure_bootstrapped).
async fn fence(cfg: &Config, new_primary: &str) {
    tracing::warn!(%new_primary, "fencing: stopping postgres, restart will rewind and rejoin");
    let _ = Command::new("pg_ctl")
        .args(["stop", "-m", "fast", "-D"])
        .arg(&cfg.pgdata)
        .status()
        .await;
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_decides_role() {
        assert_eq!(role_from_record("demo-0", "demo-0"), Role::Primary);
        assert_eq!(role_from_record("demo-2", "demo-0"), Role::Standby);
        assert_eq!(role_from_record("demo-2", "demo-2"), Role::Primary);
    }

    #[test]
    fn archive_recovery_is_told_apart_from_standby() {
        let dir = std::env::temp_dir().join("pglord-agent-test-recovery");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(!archive_recovery_in_progress(&dir));

        // A standby waiting on a primary must stay promotable: this is the
        // failover path, and blocking it would leave a cluster with no writer.
        std::fs::write(dir.join("standby.signal"), "").expect("write");
        assert!(!archive_recovery_in_progress(&dir));

        // A restore replaying WAL must not be promoted out from under itself.
        std::fs::write(dir.join("recovery.signal"), "").expect("write");
        assert!(archive_recovery_in_progress(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
