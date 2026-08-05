//! Agent status polling over headless-service DNS. The ONE code path for
//! observing instances — the orchestrator's failover decisions and the API's
//! detail view must never see different data shapes.

use pglord_core::render::{AGENT_HTTP_PORT, pod_fqdn};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AgentStatus {
    ready: bool,
    in_recovery: Option<bool>,
    lsn: Option<String>,
    #[serde(default)]
    pending_restart: bool,
    #[serde(default)]
    sync_standby_count: Option<u32>,
    #[serde(default)]
    ssl: Option<bool>,
    #[serde(default)]
    wal_level: Option<String>,
    #[serde(default)]
    server_version_num: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct InstanceObservation {
    pub pod: String,
    pub ready: bool,
    pub in_recovery: Option<bool>,
    pub lsn: Option<String>,
    /// Postgres has settings staged that only a restart will apply.
    pub pending_restart: bool,
    /// Primary only: connected standbys eligible to confirm a sync commit.
    pub sync_standby_count: Option<u32>,
    /// The server is accepting TLS connections. None when unreachable.
    pub ssl: Option<bool>,
    /// Live `wal_level`. None when unreachable, or when an older agent that
    /// does not report it answers.
    pub wal_level: Option<String>,
    /// Live `server_version_num`; the authority on which major is running.
    pub server_version_num: Option<i32>,
}

/// Observe every instance of the spec's *own* generation. During a major
/// upgrade the green set is observed separately, through its own spec view —
/// mixing the two here would let a green pod's readiness mask a blue outage.
pub async fn observe_cluster(
    http: &reqwest::Client,
    spec: &pglord_core::spec::ClusterSpec,
    ns: &str,
) -> Vec<InstanceObservation> {
    let mut out = Vec::with_capacity(spec.replicas as usize);
    for i in 0..spec.replicas {
        let pod = spec.instance_name(i);
        let url = format!(
            "http://{}:{}/status",
            pod_fqdn(&pod, &spec.name, ns),
            AGENT_HTTP_PORT
        );
        let status = match http.get(&url).send().await {
            Ok(resp) => resp.json::<AgentStatus>().await.ok(),
            Err(_) => None,
        };
        out.push(match status {
            Some(s) => InstanceObservation {
                pod,
                ready: s.ready,
                in_recovery: s.in_recovery,
                lsn: s.lsn,
                pending_restart: s.pending_restart,
                sync_standby_count: s.sync_standby_count,
                ssl: s.ssl,
                wal_level: s.wal_level,
                server_version_num: s.server_version_num,
            },
            None => InstanceObservation {
                pod,
                ready: false,
                in_recovery: None,
                lsn: None,
                pending_restart: false,
                sync_standby_count: None,
                ssl: None,
                wal_level: None,
                server_version_num: None,
            },
        });
    }
    out
}
