//! Liveness and readiness. Modules owning external deps register a named probe
//! and flip it from their own health loop; /readyz aggregates.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeStatus {
    Ready,
    Unready,
}

/// Named readiness probes. Empty registry = ready, so the server is ready
/// until a dependency-owning module registers itself unready.
#[derive(Clone, Default)]
pub struct Readiness(Arc<RwLock<BTreeMap<String, ProbeStatus>>>);

impl Readiness {
    pub async fn set(&self, name: impl Into<String>, status: ProbeStatus) {
        self.0.write().await.insert(name.into(), status);
    }

    pub async fn snapshot(&self) -> BTreeMap<String, ProbeStatus> {
        self.0.read().await.clone()
    }
}

/// Pure liveness: the process is up and serving. Never checks dependencies.
pub async fn healthz() -> StatusCode {
    StatusCode::OK
}

pub async fn readyz(State(readiness): State<Readiness>) -> impl IntoResponse {
    let probes = readiness.snapshot().await;
    let ready = probes.values().all(|s| *s == ProbeStatus::Ready);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(serde_json::json!({
            "status": if ready { "ready" } else { "unready" },
            "probes": probes,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn empty_registry_is_ready() {
        let r = Readiness::default();
        let resp = readyz(State(r)).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn one_unready_probe_flips_readyz_to_503() {
        let r = Readiness::default();
        r.set("kube", ProbeStatus::Unready).await;
        let resp = readyz(State(r.clone())).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        r.set("kube", ProbeStatus::Ready).await;
        let resp = readyz(State(r)).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }
}
