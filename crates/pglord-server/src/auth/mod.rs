//! Bearer-token authentication for the API router. One middleware, two
//! credential kinds behind the same header: operator-configured static tokens
//! (checked first, constant-time) and OIDC access tokens (validated as JWTs
//! against the issuer's JWKS — see `jwks`). Installed only when auth is
//! configured; everything outside `/api` plus the public auth-config endpoint
//! passes through untouched.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::config::Config;
use crate::error::ApiError;

pub mod jwks;
pub mod kubernetes;

/// The one anonymous API path: the UI needs it to render the login screen
/// before it has any credential.
const PUBLIC_PATH: &str = "/api/v1/auth/config";

pub struct AuthState {
    tokens: Vec<String>,
    pub oidc: Option<jwks::OidcValidator>,
    /// Kubernetes TokenReview/SAR mode; None when the mode is off or no
    /// Kubernetes environment exists (tokens then fall through to 401 —
    /// never a 500).
    pub kubernetes: Option<kubernetes::KubernetesAuthorizer>,
}

impl AuthState {
    /// None = auth disabled (no middleware installed, API runs open).
    pub fn from_config(
        cfg: &Config,
        http: reqwest::Client,
        kube: Option<kube::Client>,
    ) -> Option<Arc<AuthState>> {
        if !cfg.auth_enabled() {
            return None;
        }
        let oidc = (!cfg.auth_oidc_issuer.is_empty()).then(|| {
            jwks::OidcValidator::new(
                cfg.auth_oidc_issuer.clone(),
                cfg.auth_oidc_client_id.clone(),
                cfg.oidc_audience().to_string(),
                http,
            )
        });
        let kubernetes = match (cfg.auth_kubernetes, kube) {
            (true, Some(client)) => Some(kubernetes::KubernetesAuthorizer::new(client)),
            (true, None) => {
                tracing::warn!(
                    "PGLORD_AUTH_KUBERNETES is on but no Kubernetes environment exists;                      kubernetes tokens will be rejected"
                );
                None
            }
            _ => None,
        };
        Some(Arc::new(AuthState {
            tokens: cfg.auth_token_list(),
            oidc,
            kubernetes,
        }))
    }

    fn matches_static(&self, presented: &str) -> bool {
        let presented = presented.as_bytes();
        // Fold over all tokens instead of early-returning so timing does not
        // reveal which configured token was the near-miss.
        let mut ok = false;
        for token in &self.tokens {
            let token = token.as_bytes();
            if token.len() == presented.len() && bool::from(token.ct_eq(presented)) {
                ok = true;
            }
        }
        ok
    }
}

pub async fn middleware(
    State(auth): State<Option<Arc<AuthState>>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(auth) = auth else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    if !path.starts_with("/api") || path == PUBLIC_PATH {
        return next.run(req).await;
    }
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = bearer else {
        return ApiError::unauthorized("auth_required")
            .with_detail("This API requires an Authorization: Bearer credential.")
            .into_response();
    };
    if auth.matches_static(token) {
        return next.run(req).await;
    }
    if let Some(oidc) = &auth.oidc
        && oidc.validate(token).await.is_some()
    {
        return next.run(req).await;
    }
    if let Some(k8s) = &auth.kubernetes {
        let access = kubernetes::authz_target(req.method(), req.uri().path());
        if let Some(subject) = k8s.authenticate(token).await {
            if k8s
                .authorize(&kubernetes::token_key(token), &subject, &access)
                .await
            {
                return next.run(req).await;
            }
            return ApiError::new(axum::http::StatusCode::FORBIDDEN, "forbidden")
                .with_detail(kubernetes::denial_detail(&subject.user, &access))
                .into_response();
        }
    }
    ApiError::unauthorized("auth_invalid")
        .with_detail("The presented credential was not accepted.")
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app(auth: Option<Arc<AuthState>>) -> Router {
        Router::new()
            .route("/api/v1/ping", get(|| async { "ok" }))
            .route("/other", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(auth, middleware))
    }

    fn state(tokens: &str) -> Option<Arc<AuthState>> {
        let cfg = Config {
            auth_tokens: tokens.into(),
            ..Config::test_default()
        };
        AuthState::from_config(&cfg, reqwest::Client::new(), None)
    }

    async fn get_with(
        app: Router,
        path: &str,
        header: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = HttpRequest::get(path);
        if let Some(h) = header {
            req = req.header("authorization", h);
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
    async fn missing_header_is_401_auth_required() {
        let (status, body) = get_with(app(state("tok-a")), "/api/v1/ping", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "auth_required");
    }

    #[tokio::test]
    async fn wrong_token_is_401_auth_invalid() {
        let (status, body) =
            get_with(app(state("tok-a")), "/api/v1/ping", Some("Bearer nope")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "auth_invalid");
    }

    #[tokio::test]
    async fn right_token_passes() {
        let (status, _) = get_with(
            app(state("tok-a,tok-b")),
            "/api/v1/ping",
            Some("Bearer tok-b"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn non_api_paths_stay_open() {
        let (status, _) = get_with(app(state("tok-a")), "/other", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_config_path_stays_open() {
        // 404 from the tiny router is fine — the point is it is not 401.
        let (status, _) = get_with(app(state("tok-a")), "/api/v1/auth/config", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn disabled_auth_passes_everything() {
        let (status, _) = get_with(app(None), "/api/v1/ping", None).await;
        assert_eq!(status, StatusCode::OK);
    }
}
