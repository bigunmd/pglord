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
        .route("/metrics", get(metrics))
        .route("/promote", post(promote))
        .route("/backup", post(take_backup))
        .route("/backups", get(list_backups))
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

/// Prometheus text format. Always 200: postgres being down is reported as
/// pglord_pg_up 0, not as a scrape failure.
async fn metrics(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    let sample = crate::metrics::collect(&cfg).await;
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        crate::metrics::render(&sample),
    )
}

#[derive(serde::Deserialize, Default)]
struct BackupRequest {
    /// "full" (default), "diff" or "incr".
    #[serde(default)]
    kind: Option<String>,
}

/// Take a backup. Only the primary may: pgBackRest backs up from the instance
/// that owns the WAL timeline.
async fn take_backup(
    State(cfg): State<Arc<Config>>,
    body: Option<Json<BackupRequest>>,
) -> impl IntoResponse {
    let kind = body
        .and_then(|Json(b)| b.kind)
        .unwrap_or_else(|| "full".to_string());
    let st = status::query_status(&cfg).await;
    if st.in_recovery != Some(false) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "backups run on the primary only" })),
        );
    }
    // stanza-create is idempotent and cheap; doing it here means the first
    // backup works without a separate setup step.
    if let Err(err) = crate::backup::ensure_stanza(&cfg).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("stanza-create: {err}") })),
        );
    }
    match crate::backup::take_backup(&cfg, &kind).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "completed", "kind": kind })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        ),
    }
}

async fn list_backups(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    match crate::backup::list_backups(&cfg).await {
        Ok(backups) => (
            StatusCode::OK,
            Json(serde_json::json!({ "backups": backups })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        ),
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
