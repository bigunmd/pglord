//! The one anonymous API endpoint: what the login screen needs to know before
//! it has any credential. Issuer and client ID are public by OIDC design.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::AppState;

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
    })
}
