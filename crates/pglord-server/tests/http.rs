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

fn test_app_with(cfg: pglord_server::config::Config) -> axum::Router {
    let (router, _api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &cfg,
        None,
        Default::default(),
    );
    router
}

async fn send(
    app: axum::Router,
    path: &str,
    bearer: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::get(path);
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = resp.status();
    let body = resp.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn auth_off_reports_both_modes_disabled_and_stays_open() {
    let (status, body) = send(test_app(), "/api/v1/auth/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["static_tokens_enabled"], false);
    assert_eq!(body["oidc"], serde_json::Value::Null);
    assert_eq!(body["kubernetes_enabled"], false);

    let k8s_cfg = pglord_server::config::Config {
        auth_kubernetes: true,
        ..pglord_server::config::Config::test_default()
    };
    let (status, body) = send(test_app_with(k8s_cfg), "/api/v1/auth/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kubernetes_enabled"], true);
}

#[tokio::test]
async fn auth_on_locks_api_but_not_health_metrics_or_auth_config() {
    let cfg = || pglord_server::config::Config {
        auth_tokens: "s3cret".into(),
        ..pglord_server::config::Config::test_default()
    };

    let (status, body) = send(test_app_with(cfg()), "/api/v1/ping", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "auth_required");

    let resp = test_app_with(cfg())
        .oneshot(
            Request::get("/api/v1/ping")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.headers()["www-authenticate"], "Bearer");

    let (status, _) = send(test_app_with(cfg()), "/api/v1/ping", Some("s3cret")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(test_app_with(cfg()), "/api/v1/ping", Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "auth_invalid");

    let (status, body) = send(test_app_with(cfg()), "/api/v1/auth/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["static_tokens_enabled"], true);
    assert_eq!(body["oidc"], serde_json::Value::Null);

    for open_path in ["/healthz", "/metrics"] {
        let (status, _) = send(test_app_with(cfg()), open_path, None).await;
        assert_eq!(status, StatusCode::OK, "{open_path} must stay open");
    }
}

#[tokio::test]
async fn auth_config_reports_oidc_when_configured() {
    let cfg = pglord_server::config::Config {
        auth_oidc_issuer: "https://idp.example.com/realms/x".into(),
        auth_oidc_client_id: "pglord".into(),
        ..pglord_server::config::Config::test_default()
    };
    let (status, body) = send(test_app_with(cfg), "/api/v1/auth/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oidc"]["issuer"], "https://idp.example.com/realms/x");
    assert_eq!(body["oidc"]["client_id"], "pglord");
}

#[tokio::test]
async fn auth_config_is_in_the_openapi_contract() {
    let (_router, api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &pglord_server::config::Config::test_default(),
        None,
        Default::default(),
    );
    assert!(api.paths.paths.contains_key("/api/v1/auth/config"));
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
async fn legacy_cluster_paths_alias_to_the_own_namespace() {
    // Without kube both forms must resolve to the same handler -> same 503.
    for path in [
        "/api/v1/clusters/demo",
        "/api/v1/namespaces/pglord/clusters/demo",
    ] {
        let (status, body) = send(test_app(), path, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(body["code"], "kube_unavailable", "{path}");
    }
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
        ("POST", "/api/v1/clusters/demo/hibernate", "{}"),
        ("POST", "/api/v1/clusters/demo/wake", "{}"),
        ("POST", "/api/v1/clusters/demo/instances/demo-1/fence", "{}"),
        (
            "POST",
            "/api/v1/clusters/demo/instances/demo-1/unfence",
            "{}",
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
            .contains_key("/api/v1/namespaces/{ns}/clusters/{name}/rotate-credentials")
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
async fn tls_is_in_the_openapi_contract() {
    let (_router, api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &pglord_server::config::Config::test_default(),
        None,
        Default::default(),
    );
    let schemas = &api.components.as_ref().expect("components").schemas;
    assert!(schemas.contains_key("TlsSpec"));
    let spec_change =
        serde_json::to_string(schemas.get("SpecChange").expect("SpecChange")).expect("serialize");
    assert!(spec_change.contains("tls"));
    let detail = serde_json::to_string(schemas.get("ClusterDetail").expect("ClusterDetail"))
        .expect("serialize");
    assert!(detail.contains("tls"));
    // Per-instance ssl badge for the UI.
    let instance =
        serde_json::to_string(schemas.get("InstanceDto").expect("InstanceDto")).expect("serialize");
    assert!(instance.contains("ssl"));
    // The cert method rides the existing HbaRule schema.
    let method =
        serde_json::to_string(schemas.get("HbaMethod").expect("HbaMethod")).expect("serialize");
    assert!(method.contains("cert"));
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
