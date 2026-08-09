//! The one anonymous API endpoint: what the login screen needs to know before
//! it has any credential. Issuer and client ID are public by OIDC design.

use axum::Json;
use axum::extract::{Extension, State};
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::AppState;
use crate::auth::Identity;

#[derive(Serialize, ToSchema)]
pub struct OidcPublicConfig {
    /// OIDC issuer URL the SPA runs discovery against.
    pub issuer: String,
    /// Public client ID for the authorization-code + PKCE flow.
    pub client_id: String,
}

#[derive(Serialize, ToSchema)]
pub struct AuthConfigResponse {
    /// True when the operator configured at least one static bearer token.
    pub static_tokens_enabled: bool,
    /// Present when OIDC login is available; null otherwise.
    pub oidc: Option<OidcPublicConfig>,
    /// True when Kubernetes bearer tokens are accepted (kubectl create token).
    pub kubernetes_enabled: bool,
    /// Space-separated scopes the SPA must request. Server-side so an IdP
    /// that needs a different set does not require a UI rebuild.
    pub scopes: String,
}

/// The identity a credential resolved to, as pglord sees it.
#[derive(Serialize, ToSchema)]
pub struct WhoamiResponse {
    /// How the credential was accepted: "static", "oidc", "kubernetes", or
    /// "none" when the API runs unauthenticated.
    pub mode: String,
    /// Username authorization ran against; empty for static tokens.
    pub user: String,
    /// Groups authorization ran against.
    pub groups: Vec<String>,
    /// Human-readable name from the IdP, when it supplied one.
    pub display_name: Option<String>,
    /// True when this credential is scoped by Kubernetes RBAC rather than
    /// being a full admin.
    pub rbac: bool,
}

/// Which login affordances the UI should render. When both modes are off the
/// API runs unauthenticated and the UI skips login entirely.
#[utoipa::path(
    get,
    path = "/api/v1/auth/config",
    tag = "system",
    responses((status = 200, description = "Available authentication modes", body = AuthConfigResponse))
)]
pub async fn auth_config(State(state): State<AppState>) -> Json<AuthConfigResponse> {
    let cfg = &state.cfg;
    Json(AuthConfigResponse {
        static_tokens_enabled: !cfg.auth_token_list().is_empty(),
        oidc: (!cfg.auth_oidc_issuer.is_empty()).then(|| OidcPublicConfig {
            issuer: cfg.auth_oidc_issuer.clone(),
            client_id: cfg.auth_oidc_client_id.clone(),
        }),
        kubernetes_enabled: cfg.auth_kubernetes,
        scopes: cfg.auth_oidc_scopes.clone(),
    })
}

/// Who the presented credential resolved to. Authenticated, unlike
/// `/auth/config` — it reports an identity. When a namespace 403s
/// unexpectedly this is what shows whether the mapped username matches the
/// RoleBinding: `oidc:alice@corp.com` and `alice@corp.com` are different
/// subjects, and RBAC matching is byte-exact.
#[utoipa::path(
    get,
    path = "/api/v1/auth/whoami",
    tag = "system",
    responses((status = 200, description = "The identity of the caller", body = WhoamiResponse))
)]
pub async fn whoami(identity: Option<Extension<Identity>>) -> Json<WhoamiResponse> {
    let Some(Extension(identity)) = identity else {
        // No middleware ran: the API is open.
        return Json(WhoamiResponse {
            mode: "none".into(),
            user: String::new(),
            groups: Vec::new(),
            display_name: None,
            rbac: false,
        });
    };
    let (user, groups) = identity
        .subject
        .map(|s| (s.user, s.groups))
        .unwrap_or_default();
    Json(WhoamiResponse {
        mode: identity.mode.to_string(),
        user,
        groups,
        display_name: identity.display_name,
        rbac: identity.rbac,
    })
}
