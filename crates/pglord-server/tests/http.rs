//! Integration tests exercising the exact router main() serves, via app::build.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn test_app() -> axum::Router {
    let cfg = pglord_server::config::Config::test_default();
    let (router, _api) =
        pglord_server::app::build(pglord_server::health::Readiness::default(), &cfg, None);
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
    );
    assert!(api.paths.paths.contains_key("/api/v1/ping"));
    assert!(api.paths.paths.contains_key("/api/v1/clusters"));
    assert!(!api.paths.paths.contains_key("/healthz"));
}
