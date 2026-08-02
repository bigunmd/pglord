//! API error type. Handlers return `Result<_, ApiError>`; the body is an
//! RFC 9457 problem document. `code` is the stable machine contract; `detail`
//! is human text and may change freely. `source` is logged, never serialized.

use std::borrow::Cow;
use std::error::Error;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

const PROBLEM_BASE_URL: &str = "https://pglord.dev/problems";

#[derive(Debug, Serialize, ToSchema)]
pub struct ProblemDetails {
    /// URI reference identifying the problem type: `{PROBLEM_BASE_URL}/{code}`.
    pub r#type: String,
    /// Canonical reason phrase for the status.
    pub title: String,
    pub status: u16,
    /// Stable machine-readable code; clients must key on this, not `detail`.
    pub code: String,
    /// Human-readable explanation; not part of the stable contract.
    pub detail: String,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: Cow<'static, str>,
    detail: Cow<'static, str>,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: impl Into<Cow<'static, str>>) -> Self {
        ApiError {
            status,
            code: code.into(),
            detail: Cow::Borrowed(""),
            source: None,
        }
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found")
            .with_detail("The requested resource does not exist.")
    }

    pub fn unauthorized(code: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code)
    }

    pub fn bad_request(code: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }

    pub fn conflict(code: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::CONFLICT, code)
    }

    pub fn internal(source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        let mut e = Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal")
            .with_detail("An internal error occurred.");
        e.source = Some(source.into());
        e
    }

    pub fn with_detail(mut self, detail: impl Into<Cow<'static, str>>) -> Self {
        self.detail = detail.into();
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(code = %self.code, source = ?self.source, "request failed");
        } else {
            tracing::debug!(code = %self.code, "request rejected");
        }
        let problem = ProblemDetails {
            r#type: format!("{PROBLEM_BASE_URL}/{}", self.code),
            title: self
                .status
                .canonical_reason()
                .unwrap_or("Error")
                .to_string(),
            status: self.status.as_u16(),
            code: self.code.into_owned(),
            detail: self.detail.into_owned(),
        };
        let mut resp = (self.status, axum::Json(problem)).into_response();
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        // RFC 6750: a 401 must tell the client which scheme the API expects.
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer"),
            );
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn not_found_renders_problem_json() {
        let resp = ApiError::not_found().into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers()[axum::http::header::CONTENT_TYPE],
            "application/problem+json"
        );
        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["status"], 404);
        assert_eq!(v["code"], "not_found");
        assert_eq!(v["type"], "https://pglord.dev/problems/not_found");
    }

    #[tokio::test]
    async fn unauthorized_carries_www_authenticate_bearer() {
        let resp = ApiError::unauthorized("auth_required").into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()[axum::http::header::WWW_AUTHENTICATE],
            "Bearer"
        );
    }

    #[tokio::test]
    async fn internal_error_hides_source_from_body() {
        let resp = ApiError::internal(std::io::Error::other("secret db path")).into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let body = resp.into_body().collect().await.expect("body").to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("secret db path"));
    }
}
