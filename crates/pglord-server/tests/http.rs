//! Integration tests exercising the exact router main() serves, via app::build.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn test_app() -> axum::Router {
    let cfg = pglord_server::config::Config::test_default();
    let (router, _api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &cfg,
        None,
        Default::default(),
    );
    router
}

#[tokio::test]
async fn clusters_without_kube_answer_503_problem() {
    let resp = test_app()
        .oneshot(
            Request::post("/api/v1/clusters")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"demo"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.headers()["content-type"], "application/problem+json");
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["code"], "kube_unavailable");
}

#[tokio::test]
async fn ping_returns_pong() {
    let resp = test_app()
        .oneshot(
            Request::get("/api/v1/ping")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["message"], "pong");
}

#[tokio::test]
async fn version_reports_workspace_version() {
    let resp = test_app()
        .oneshot(
            Request::get("/api/v1/version")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["version"], pglord_core::VERSION);
}

#[tokio::test]
async fn day2_operations_without_kube_answer_503_problem() {
    for (method, path, body) in [
        ("PATCH", "/api/v1/clusters/demo", r#"{"replicas":3}"#),
        (
            "POST",
            "/api/v1/clusters/demo/switchover",
            r#"{"target":"demo-1"}"#,
        ),
    ] {
        let resp = test_app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(
            resp.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path}"
        );
        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["code"], "kube_unavailable");
    }
}

#[tokio::test]
async fn unknown_api_path_is_problem_json_404() {
    let resp = test_app()
        .oneshot(
            Request::get("/api/v1/nope")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(resp.headers()["content-type"], "application/problem+json");
}

#[tokio::test]
async fn healthz_and_readyz_live_outside_api_contract() {
    let resp = test_app()
        .oneshot(
            Request::get("/healthz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let (_router, api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &pglord_server::config::Config::test_default(),
        None,
        Default::default(),
    );
    assert!(api.paths.paths.contains_key("/api/v1/ping"));
    assert!(api.paths.paths.contains_key("/api/v1/clusters"));
    assert!(!api.paths.paths.contains_key("/healthz"));
}

#[tokio::test]
async fn rotate_credentials_without_kube_answers_503() {
    let resp = test_app()
        .oneshot(
            Request::post("/api/v1/clusters/demo/rotate-credentials")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn rotate_credentials_is_in_the_openapi_contract() {
    let (_router, api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &pglord_server::config::Config::test_default(),
        None,
        Default::default(),
    );
    assert!(
        api.paths
            .paths
            .contains_key("/api/v1/clusters/{name}/rotate-credentials")
    );
}

#[tokio::test]
async fn pg_hba_is_in_the_openapi_contract() {
    let (_router, api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &pglord_server::config::Config::test_default(),
        None,
        Default::default(),
    );
    let schemas = &api.components.as_ref().expect("components").schemas;
    assert!(schemas.contains_key("HbaRule"));
    let spec_change =
        serde_json::to_string(schemas.get("SpecChange").expect("SpecChange")).expect("serialize");
    assert!(spec_change.contains("pg_hba"));
    let detail = serde_json::to_string(schemas.get("ClusterDetail").expect("ClusterDetail"))
        .expect("serialize");
    assert!(detail.contains("pg_hba"));
}

#[tokio::test]
async fn metrics_serves_prometheus_text() {
    let resp = test_app()
        .oneshot(
            Request::get("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("pglord_clusters"));
}

#[tokio::test]
async fn metrics_is_not_in_the_openapi_contract() {
    let (_router, api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &pglord_server::config::Config::test_default(),
        None,
        Default::default(),
    );
    assert!(!api.paths.paths.contains_key("/metrics"));
}
