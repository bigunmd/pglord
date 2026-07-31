//! Kubernetes client bootstrap and apiserver readiness probe.

use std::time::Duration;

use crate::health::{ProbeStatus, Readiness};

pub const PROBE_NAME: &str = "kube";

/// Infer a client from the environment (in-cluster service account or local
/// kubeconfig). Returns None (with a loud log line) when neither is available
/// so the API can still serve — /readyz stays unready via the probe.
pub async fn init(readiness: &Readiness) -> Option<kube::Client> {
    readiness.set(PROBE_NAME, ProbeStatus::Unready).await;
    match kube::Client::try_default().await {
        Ok(client) => Some(client),
        Err(err) => {
            tracing::warn!(%err, "no Kubernetes environment; cluster management disabled");
            None
        }
    }
}

/// Current namespace: in-cluster service account file, or PGLORD_NAMESPACE,
/// or "default".
pub fn current_namespace() -> String {
    if let Ok(ns) = std::env::var("PGLORD_NAMESPACE") {
        return ns;
    }
    std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "default".to_string())
}

/// Flip the `kube` probe based on apiserver reachability.
pub async fn health_loop(client: kube::Client, readiness: Readiness) {
    loop {
        let status = match client.apiserver_version().await {
            Ok(_) => ProbeStatus::Ready,
            Err(err) => {
                tracing::warn!(%err, "apiserver health check failed");
                ProbeStatus::Unready
            }
        };
        readiness.set(PROBE_NAME, status).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
