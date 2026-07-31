//! Internal control endpoint. Consumed by pglord-server (probes, failover
//! orchestration) — deliberately NOT part of the public OpenAPI contract.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::process::Command;

use crate::config::Config;
use crate::status;

pub fn router(cfg: Arc<Config>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(readyz))
        .route("/status", get(get_status))
        .route("/promote", post(promote))
        .with_state(cfg)
}

async fn get_status(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    Json(status::query_status(&cfg).await)
}

/// Pod readinessProbe target: ready only when postgres accepts connections.
async fn readyz(State(cfg): State<Arc<Config>>) -> StatusCode {
    if status::query_status(&cfg).await.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn promote(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    let st = status::query_status(&cfg).await;
    if st.in_recovery != Some(true) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "not in recovery; nothing to promote" })),
        );
    }
    match Command::new("pg_ctl")
        .arg("promote")
        .arg("-D")
        .arg(&cfg.pgdata)
        .status()
        .await
    {
        Ok(s) if s.success() => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "promoting" })),
        ),
        Ok(s) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("pg_ctl promote failed: {s}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("spawn pg_ctl: {e}") })),
        ),
    }
}
