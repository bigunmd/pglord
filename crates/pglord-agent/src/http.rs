//! Internal control endpoint. Consumed by pglord-server (probes, failover
//! orchestration) — deliberately NOT part of the public OpenAPI contract.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
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
        .route("/roles/apply", post(apply_roles))
        .route("/pooler/ensure", post(ensure_pooler))
        .route("/provision/apply", post(apply_provisioning))
        .route("/backup", post(take_backup))
        .route("/backups", get(list_backups))
        .route("/backup/stanza-upgrade", post(stanza_upgrade))
        .route("/upgrade/status", get(upgrade_status))
        .route("/upgrade/cutover", post(upgrade_cutover))
        .route("/upgrade/prepare", post(upgrade_prepare))
        .route("/upgrade/fence", post(upgrade_fence))
        .route("/upgrade/unfence", post(upgrade_unfence))
        .route("/upgrade/cleanup", post(upgrade_cleanup))
        .with_state(cfg)
}

/// Primary-only gate shared by every mutating endpoint: "not provably the
/// primary" (including postgres unreachable) is a refusal, never a maybe.
fn require_primary(st: &status::InstanceStatus, what: &str) -> Option<(StatusCode, Json<Value>)> {
    (st.in_recovery != Some(false)).then(|| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": format!("{what} runs on the primary only") })),
        )
    })
}

fn json_result(result: anyhow::Result<()>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))),
        // {:#}, not {}: a tokio_postgres failure renders as a bare "db error"
        // without its cause chain, which tells the operator nothing.
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("{err:#}") })),
        ),
    }
}

/// After a major upgrade the stanza still describes the old PostgreSQL
/// version; stanza-upgrade re-points it at the current data directory.
async fn stanza_upgrade(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    let st = status::query_status(&cfg).await;
    if let Some(refusal) = require_primary(&st, "stanza-upgrade") {
        return refusal;
    }
    json_result(crate::backup::ensure_stanza_upgrade(&cfg).await)
}

/// Migration progress, from the new instance set's point of view.
async fn upgrade_status(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    if cfg.upgrade_source.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not an upgrade target instance" })),
        );
    }
    let st = crate::upgrade::status(&cfg).await;
    (
        StatusCode::OK,
        Json(serde_json::to_value(st).unwrap_or_default()),
    )
}

async fn upgrade_cutover(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    if cfg.upgrade_source.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not an upgrade target instance" })),
        );
    }
    let st = status::query_status(&cfg).await;
    if let Some(refusal) = require_primary(&st, "cutover") {
        return refusal;
    }
    json_result(crate::upgrade::cutover(&cfg).await)
}

#[derive(serde::Deserialize)]
struct WalRetentionRequest {
    max_slot_wal_keep_size: String,
}

/// Bound the WAL a migration may pin here, before its slot is created.
async fn upgrade_prepare(
    State(cfg): State<Arc<Config>>,
    Json(req): Json<WalRetentionRequest>,
) -> impl IntoResponse {
    // Reject before touching postgres, and with the same 400 shape the other
    // endpoints use for a value that fails its charset check.
    if !pglord_core::upgrade::is_valid_wal_size(&req.max_slot_wal_keep_size) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "max_slot_wal_keep_size must be a size literal" })),
        );
    }
    let st = status::query_status(&cfg).await;
    if let Some(refusal) = require_primary(&st, "upgrade preparation") {
        return refusal;
    }
    json_result(crate::upgrade::set_wal_retention(&cfg, &req.max_slot_wal_keep_size).await)
}

async fn upgrade_fence(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    let st = status::query_status(&cfg).await;
    if let Some(refusal) = require_primary(&st, "fencing writes") {
        return refusal;
    }
    json_result(crate::upgrade::fence_writes(&cfg).await)
}

async fn upgrade_unfence(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    let st = status::query_status(&cfg).await;
    if let Some(refusal) = require_primary(&st, "unfencing writes") {
        return refusal;
    }
    json_result(crate::upgrade::unfence_writes(&cfg).await)
}

async fn upgrade_cleanup(State(cfg): State<Arc<Config>>) -> impl IntoResponse {
    let st = status::query_status(&cfg).await;
    if let Some(refusal) = require_primary(&st, "upgrade cleanup") {
        return refusal;
    }
    json_result(crate::upgrade::cleanup_source_artifacts(&cfg).await)
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
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
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

#[derive(serde::Deserialize)]
struct RolesRequest {
    superuser_password: String,
    repl_password: String,
}

/// Apply new role passwords. Primary only: standbys' pg_authid is replicated.
/// Charset is validated before any postgres access so a bad request is 400
/// even when postgres is down.
async fn apply_roles(
    State(cfg): State<Arc<Config>>,
    Json(req): Json<RolesRequest>,
) -> impl IntoResponse {
    if !crate::postboot::is_safe_password(&req.superuser_password)
        || !crate::postboot::is_safe_password(&req.repl_password)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "passwords may only contain [A-Za-z0-9_-]" })),
        );
    }
    let st = status::query_status(&cfg).await;
    if st.in_recovery != Some(false) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "role changes run on the primary only" })),
        );
    }
    match crate::postboot::apply_passwords(&cfg, &req.superuser_password, &req.repl_password).await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "applied" })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        ),
    }
}

#[derive(serde::Deserialize)]
struct PoolerEnsureRequest {
    password: String,
}

/// Ensure the pooler auth role and lookup function. Primary only, same guard
/// as `/roles/apply`: role and function DDL replicate to standbys.
async fn ensure_pooler(
    State(cfg): State<Arc<Config>>,
    Json(req): Json<PoolerEnsureRequest>,
) -> impl IntoResponse {
    if !crate::postboot::is_safe_password(&req.password) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "passwords may only contain [A-Za-z0-9_-]" })),
        );
    }
    let st = status::query_status(&cfg).await;
    if st.in_recovery != Some(false) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "pooler setup runs on the primary only" })),
        );
    }
    match crate::postboot::ensure_pooler(&cfg, &req.password).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "applied" })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        ),
    }
}

/// Converge declared roles and databases. Primary only, same guard as the
/// other DDL endpoints; every name and password is charset-checked before
/// any postgres access so a bad request is 400 even when postgres is down.
async fn apply_provisioning(
    State(cfg): State<Arc<Config>>,
    Json(req): Json<crate::postboot::ProvisionRequest>,
) -> impl IntoResponse {
    // Full spec validation, not just charsets: without the reserved-name
    // checks, anything on the pod network could ask this endpoint to ALTER
    // `postgres` or `replicator`. Never trust the caller to have validated.
    let (role_specs, db_specs, ext_specs) = req.to_specs();
    if let Err(err) = pglord_core::provision::validate_all(&role_specs, &db_specs, &ext_specs) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": err.to_string() })),
        );
    }
    let passwords_ok = req
        .roles
        .iter()
        .filter_map(|r| r.password.as_deref())
        .all(crate::postboot::is_safe_password);
    if !passwords_ok {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "passwords may only contain [A-Za-z0-9_-]" })),
        );
    }
    let st = status::query_status(&cfg).await;
    if st.in_recovery != Some(false) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "provisioning runs on the primary only" })),
        );
    }
    match crate::postboot::apply_provisioning(&cfg, &req).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "applied" })),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_router() -> Router {
        let cfg: Config =
            serde_json::from_value(serde_json::json!({ "role": "primary" })).expect("test config");
        router(Arc::new(cfg))
    }

    #[tokio::test]
    async fn roles_apply_rejects_unsafe_passwords_without_touching_postgres() {
        let resp = test_router()
            .oneshot(
                Request::post("/roles/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"superuser_password":"has$pglord$end","repl_password":"ok-pw_1"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn pooler_ensure_rejects_unsafe_passwords_without_touching_postgres() {
        let resp = test_router()
            .oneshot(
                Request::post("/pooler/ensure")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"has$pglord$end"}"#))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn pooler_ensure_refuses_when_not_provably_primary() {
        // No postgres in unit tests: status.in_recovery is None, which must
        // read as "not the primary" and refuse.
        let resp = test_router()
            .oneshot(
                Request::post("/pooler/ensure")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"aB3-x_9"}"#))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn provision_rejects_unsafe_input_without_touching_postgres() {
        let resp = test_router()
            .oneshot(
                Request::post("/provision/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"roles":[{"name":"app_rw","password":"has$pglord$end"}],"databases":[]}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = test_router()
            .oneshot(
                Request::post("/provision/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"roles":[{"name":"Bad-Name"}],"databases":[]}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn provision_rejects_reserved_and_dangling_names_agent_side() {
        // Defense in depth: `postgres` passes the identifier charset, so a
        // charset-only check would let a pod-network caller ALTER the
        // superuser. The full validation must run here, not just on the
        // server.
        for body in [
            r#"{"roles":[{"name":"postgres","password":"pw-1"}],"databases":[]}"#,
            r#"{"roles":[{"name":"replicator"}],"databases":[]}"#,
            r#"{"roles":[{"name":"pg_sneaky"}],"databases":[]}"#,
            r#"{"roles":[],"databases":[{"name":"appdb","owner":"ghost"}]}"#,
        ] {
            let resp = test_router()
                .oneshot(
                    Request::post("/provision/apply")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "accepted: {body}");
        }
    }

    #[tokio::test]
    async fn provision_refuses_when_not_provably_primary() {
        let resp = test_router()
            .oneshot(
                Request::post("/provision/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"roles":[{"name":"app_rw"}],"databases":[]}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn roles_apply_refuses_when_not_provably_primary() {
        // No postgres in unit tests: status.in_recovery is None, which must
        // read as "not the primary" and refuse.
        let resp = test_router()
            .oneshot(
                Request::post("/roles/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"superuser_password":"aB3-x_9","repl_password":"cD4-y_8"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }
}
