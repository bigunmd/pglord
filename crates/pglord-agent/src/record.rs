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
        let i_am_recorded = recorded == me;
        match st.in_recovery {
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

/// Stop postgres, rewind PGDATA to the new primary's timeline, prepare
/// standby files, and exit — the restarted pod comes back as a standby.
async fn fence(cfg: &Config, new_primary: &str) {
    let _ = Command::new("pg_ctl")
        .args(["stop", "-m", "fast", "-D"])
        .arg(&cfg.pgdata)
        .status()
        .await;

    let source = format!(
        "host={} port={} user={} password={} dbname=postgres",
        pod_fqdn(new_primary, &cfg.cluster, &cfg.namespace),
        cfg.primary_port,
        cfg.superuser,
        cfg.superuser_password,
    );
    let rewound = Command::new("pg_rewind")
        .arg(format!("--target-pgdata={}", cfg.pgdata.display()))
        .arg(format!("--source-server={source}"))
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);

    if rewound {
        let conninfo = format!(
            "primary_conninfo = 'host={} port={} user={} password={} application_name={}'\n",
            pod_fqdn(new_primary, &cfg.cluster, &cfg.namespace),
            cfg.primary_port,
            if cfg.repl_password.is_empty() {
                &cfg.superuser
            } else {
                &cfg.repl_user
            },
            if cfg.repl_password.is_empty() {
                &cfg.superuser_password
            } else {
                &cfg.repl_password
            },
            std::env::var("HOSTNAME").unwrap_or_default(),
        );
        let auto_conf = cfg.pgdata.join("postgresql.auto.conf");
        let append_ok = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&auto_conf)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(conninfo.as_bytes())
            })
            .is_ok();
        let signal_ok = std::fs::write(cfg.pgdata.join("standby.signal"), "").is_ok();
        if append_ok && signal_ok {
            tracing::warn!("fenced: rewound and prepared as standby; exiting for restart");
            std::process::exit(1);
        }
    }

    // Rewind (or standby prep) failed: wipe PGDATA so the restarted agent
    // re-clones with pg_basebackup.
    tracing::warn!("fence fallback: wiping PGDATA for fresh basebackup");
    let _ = std::fs::remove_dir_all(&cfg.pgdata);
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
}
