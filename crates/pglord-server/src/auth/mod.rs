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

/// How the credential on a request was accepted, and as whom. Inserted into
/// request extensions by every accepting branch; `whoami` is its only reader
/// today, and it is the seam any future audit logging needs.
#[derive(Debug, Clone)]
pub struct Identity {
    /// "static" | "oidc" | "kubernetes".
    pub mode: &'static str,
    /// The subject authorization ran against; None for static tokens.
    pub subject: Option<kubernetes::Subject>,
    pub display_name: Option<String>,
    /// True when this credential is authorized per-namespace by RBAC rather
    /// than being a full admin.
    pub rbac: bool,
}

pub struct AuthState {
    tokens: Vec<String>,
    pub oidc: Option<jwks::OidcValidator>,
    /// Claims -> Kubernetes subject mapping; Some when OIDC RBAC is on.
    pub oidc_rbac: Option<kubernetes::SubjectMapping>,
    /// Kubernetes bearer tokens are accepted (TokenReview).
    pub k8s_tokens: bool,
    /// The review engine, shared by both RBAC paths. None when no Kubernetes
    /// environment exists — tokens then fall through to 401, never a 500.
    pub reviews: Option<kubernetes::KubernetesAuthorizer>,
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
        // One review engine serves both RBAC paths.
        let wants_reviews = cfg.auth_kubernetes || cfg.auth_oidc_rbac;
        let reviews = match (wants_reviews, kube) {
            (true, Some(client)) => Some(kubernetes::KubernetesAuthorizer::new(client)),
            (true, None) => {
                tracing::warn!(
                    "Kubernetes-backed authorization is on but no Kubernetes environment \
                     exists; credentials that depend on it will be rejected"
                );
                None
            }
            _ => None,
        };
        Some(Arc::new(AuthState {
            tokens: cfg.auth_token_list(),
            oidc,
            oidc_rbac: cfg
                .auth_oidc_rbac
                .then(|| kubernetes::SubjectMapping::new(cfg)),
            k8s_tokens: cfg.auth_kubernetes,
            reviews,
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

/// Tag the request with the identity that got it in, then let it through.
async fn accept(mut req: Request, next: Next, identity: Identity) -> Response {
    req.extensions_mut().insert(identity);
    next.run(req).await
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
        return accept(
            req,
            next,
            Identity {
                mode: "static",
                subject: None,
                display_name: None,
                rbac: false,
            },
        )
        .await;
    }
    if let Some(oidc) = &auth.oidc
        && let Some(claims) = oidc.validate(token).await
    {
        let Some(mapping) = &auth.oidc_rbac else {
            // RBAC off: a valid JWT is a full admin, as it has always been.
            return accept(
                req,
                next,
                Identity {
                    mode: "oidc",
                    subject: None,
                    display_name: None,
                    rbac: false,
                },
            )
            .await;
        };
        let (Some(subject), Some(reviews)) = (mapping.subject(&claims), &auth.reviews) else {
            return ApiError::unauthorized("auth_invalid")
                .with_detail(
                    "The token carries no usable identity, or Kubernetes authorization \
                     is unavailable.",
                )
                .into_response();
        };
        let access = kubernetes::authz_target(req.method(), req.uri().path());
        if reviews
            .authorize(&kubernetes::token_key(token), &subject, &access)
            .await
        {
            let display_name = mapping.display_name(&claims);
            return accept(
                req,
                next,
                Identity {
                    mode: "oidc",
                    subject: Some(subject),
                    display_name,
                    rbac: true,
                },
            )
            .await;
        }
        return ApiError::new(axum::http::StatusCode::FORBIDDEN, "forbidden")
            .with_detail(kubernetes::denial_detail(&subject.user, &access))
            .into_response();
    }
    if let Some(k8s) = auth.reviews.as_ref().filter(|_| auth.k8s_tokens) {
        let access = kubernetes::authz_target(req.method(), req.uri().path());
        if let Some(subject) = k8s.authenticate(token).await {
            if k8s
                .authorize(&kubernetes::token_key(token), &subject, &access)
                .await
            {
                return accept(
                    req,
                    next,
                    Identity {
                        mode: "kubernetes",
                        subject: Some(subject),
                        display_name: None,
                        rbac: true,
                    },
                )
                .await;
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

    fn oidc_cfg(rbac: bool) -> Config {
        Config {
            auth_oidc_issuer: "https://idp.example.com".into(),
            auth_oidc_client_id: "pglord".into(),
            auth_oidc_rbac: rbac,
            ..Config::test_default()
        }
    }

    #[tokio::test]
    async fn oidc_rbac_without_kube_env_is_401_not_500() {
        // The stage-C constraint, now on the OIDC path: an unreachable review
        // API means "not authenticated", never a server error.
        let auth = AuthState::from_config(&oidc_cfg(true), reqwest::Client::new(), None);
        let state = auth.as_ref().expect("auth enabled");
        assert!(state.oidc_rbac.is_some());
        assert!(state.reviews.is_none(), "no kube env, no review engine");
        let (status, body) = get_with(app(auth), "/api/v1/ping", Some("Bearer a.b.c")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "auth_invalid");
    }

    #[tokio::test]
    async fn oidc_rbac_off_keeps_full_admin() {
        // Asserts CURRENT behavior, not desired behavior: with RBAC off a
        // valid JWT is still a full admin. Delete this the day the default
        // flips. The 401 here is only because the mock issuer is unreachable;
        // what matters is that the branch never reaches an authorization
        // check at all.
        let auth = AuthState::from_config(&oidc_cfg(false), reqwest::Client::new(), None);
        let state = auth.as_ref().expect("auth enabled");
        assert!(state.oidc.is_some());
        assert!(state.oidc_rbac.is_none(), "no mapping, so no SAR");
        assert!(!state.k8s_tokens, "OIDC alone must not accept SA tokens");
    }

    #[tokio::test]
    async fn static_token_carries_a_static_identity() {
        let auth = state("tok-a").expect("auth enabled");
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = captured.clone();
        let app = Router::new()
            .route(
                "/api/v1/ping",
                get(move |req: HttpRequest<Body>| {
                    let sink = sink.clone();
                    async move {
                        if let Ok(mut s) = sink.lock() {
                            *s = req.extensions().get::<Identity>().cloned();
                        }
                        "ok"
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(Some(auth), middleware));
        let (status, _) = get_with(app, "/api/v1/ping", Some("Bearer tok-a")).await;
        assert_eq!(status, StatusCode::OK);
        let identity = captured.lock().expect("lock").clone().expect("identity");
        assert_eq!(identity.mode, "static");
        assert!(!identity.rbac);
        assert!(identity.subject.is_none());
    }
}
