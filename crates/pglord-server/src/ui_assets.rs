//! Embedded Angular SPA, compiled in behind the `embed-ui` feature so plain
//! `cargo build` never requires the UI dist to exist. Docker builds enable it.

use axum::http::StatusCode;
#[cfg(feature = "embed-ui")]
use axum::http::header;
use axum::response::{IntoResponse, Response};

#[cfg(feature = "embed-ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist/pglord-ui/browser/"]
struct Assets;

/// Serve an embedded asset; unknown paths fall back to index.html so SPA
/// routes deep-link correctly.
#[cfg(feature = "embed-ui")]
pub fn serve(path: &str) -> Response {
    let path = path.trim_start_matches('/');
    let candidate = if path.is_empty() { "index.html" } else { path };
    let file = Assets::get(candidate).or_else(|| Assets::get("index.html"));
    match file {
        Some(content) => {
            let mime = mime_guess::from_path(if Assets::get(candidate).is_some() {
                candidate
            } else {
                "index.html"
            })
            .first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "UI assets missing from build").into_response(),
    }
}

#[cfg(not(feature = "embed-ui"))]
pub fn serve(_path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        "UI not embedded in this build (enable the embed-ui feature)",
    )
        .into_response()
}
