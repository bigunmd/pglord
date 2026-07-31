# pglord Foundation Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the pglord Rust workspace and a fully-wired axum application server (config, health probes, problem+json errors, code-first OpenAPI, Makefile, Docker, CI) plus a minikube dev stand.

**Architecture:** Mirrors `~/github/signal`'s proven wiring: `main.rs` owns process lifecycle, `app.rs` is the single composition seam returning `(Router, OpenApi)`, used by `main`, `--dump-openapi`, and tests. Figment-layered config (`PGLORD_*` env), Readiness probe registry behind `/readyz`, RFC 9457 `application/problem+json` errors, utoipa code-first spec committed to `spec/openapi.yaml` with a CI drift gate (a gap signal has that we close).

**Tech Stack:** Rust (edition 2024, stable toolchain), axum 0.8, utoipa 5 (+utoipa-axum, +utoipa-swagger-ui), figment, tokio, tower-http, tracing; cargo-chef + distroless Docker; minikube (docker driver) dev stand.

## Global Constraints

- License: **AGPL-3.0-or-later** (LICENSE file at repo root; SPDX in Cargo metadata).
- Crate names: `pglord-core`, `pglord-server` (dir `crates/`); binary `pglord-server`.
- Env prefix `PGLORD_`, config file `config.toml`, flat snake_case fields, every field has a `default_*` fn.
- OpenAPI artifact: `spec/openapi.yaml`, generated only via `--dump-openapi`, never hand-edited; CI fails on drift.
- Only `/api/v1/*` is in the OpenAPI contract; `/healthz`, `/readyz`, `/swagger-ui` are excluded by construction.
- Handlers return `Result<_, ApiError>`; error bodies are `application/problem+json` with a stable `code` string.
- `make lint` = `cargo fmt --check` + `clippy --all-targets -- -D warnings`; both must pass before every commit.
- Commit style: conventional commits (`feat:`, `chore:`, `ci:`, `test:`).

---

### Task 1: Repo scaffold + workspace

**Files:**
- Create: `LICENSE`, `.gitignore`, `rust-toolchain.toml`, `Cargo.toml`, `README.md`
- Create: `crates/pglord-core/{Cargo.toml,src/lib.rs}`
- Create: `crates/pglord-server/{Cargo.toml,src/main.rs}` (hello-world placeholder, replaced in Task 6)

**Interfaces:**
- Produces: workspace layout all later tasks assume; `pglord_core::VERSION: &str`.

- [ ] **Step 1: License + git basics**

```bash
cd /home/bigunmd/github/pglord
curl -fsSL https://www.gnu.org/licenses/agpl-3.0.txt -o LICENSE
printf 'target/\nspec/.tmp/\n' > .gitignore
printf '[toolchain]\nchannel = "stable"\ncomponents = ["rustfmt", "clippy"]\n' > rust-toolchain.toml
```

- [ ] **Step 2: Workspace manifest**

`Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = ["crates/pglord-core", "crates/pglord-server"]

[workspace.package]
version = "0.1.0"
edition = "2024"
authors = ["Mikhail Bigun <mikebigun@gmail.com>"]
license = "AGPL-3.0-or-later"
repository = "https://github.com/bigunmd/pglord"
publish = false

[workspace.lints.clippy]
unwrap_used = "warn"
```

- [ ] **Step 3: Crates**

`crates/pglord-core/Cargo.toml`:

```toml
[package]
name = "pglord-core"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true
publish.workspace = true

[lints]
workspace = true
```

`crates/pglord-core/src/lib.rs`:

```rust
//! Shared pglord domain types. Grows into the cluster spec model (see design doc).

/// Version of the pglord workspace, surfaced by the `/api/v1/version` endpoint.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
```

`crates/pglord-server/Cargo.toml` (placeholder deps; Task 3+ extend):

```toml
[package]
name = "pglord-server"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true
publish.workspace = true

[lints]
workspace = true

[dependencies]
pglord-core = { path = "../pglord-core" }
```

`crates/pglord-server/src/main.rs`: `fn main() { println!("pglord-server {}", pglord_core::VERSION); }`

- [ ] **Step 4: Verify + commit**

Run: `cargo build --workspace && cargo run -p pglord-server` → prints `pglord-server 0.1.0`.

```bash
git add -A && git commit -m "chore: scaffold AGPL-3.0 cargo workspace (pglord-core, pglord-server)"
```

(This first commit also includes `docs/superpowers/specs/2026-07-31-pglord-design.md` and this plan.)

---

### Task 2: Config module (figment)

**Files:**
- Create: `crates/pglord-server/src/config.rs`
- Modify: `crates/pglord-server/Cargo.toml`, `src/main.rs` (add `mod config;`)

**Interfaces:**
- Produces: `Config::load() -> anyhow::Result<Config>`; fields `http_addr: SocketAddr`, `log_level: String`, `shutdown_timeout_secs: u64`, `cors_allowed_origins: String`; accessors `shutdown_timeout() -> Duration`, `cors_origins() -> Vec<HeaderValue>`; `Config::test_default()`.

- [ ] **Step 1: Add deps**

Append to `[dependencies]`: `anyhow = "1"`, `figment = { version = "0.10", features = ["toml", "env"] }`, `serde = { version = "1", features = ["derive"] }`, `axum = "0.8"` (for `http::HeaderValue`).
`[dev-dependencies]`: `figment = { version = "0.10", features = ["toml", "env", "test"] }`.

- [ ] **Step 2: Write failing tests** (bottom of `config.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_load_without_env_or_file() {
        figment::Jail::expect_with(|_jail| {
            let cfg = Config::load().expect("defaults must load");
            assert_eq!(cfg.http_addr.to_string(), "0.0.0.0:8080");
            assert_eq!(cfg.log_level, "info");
            assert_eq!(cfg.shutdown_timeout().as_secs(), 25);
            assert!(cfg.cors_origins().is_empty());
            Ok(())
        });
    }

    #[test]
    fn env_overrides_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("PGLORD_HTTP_ADDR", "127.0.0.1:9999");
            jail.set_env("PGLORD_CORS_ALLOWED_ORIGINS", "http://a.dev, http://b.dev");
            let cfg = Config::load().expect("env override must load");
            assert_eq!(cfg.http_addr.to_string(), "127.0.0.1:9999");
            assert_eq!(cfg.cors_origins().len(), 2);
            Ok(())
        });
    }
}
```

- [ ] **Step 3: Run to verify failure** — `cargo test -p pglord-server config` → compile error (`Config` undefined).

- [ ] **Step 4: Implement**

```rust
//! Startup configuration. Layered: serde defaults <- config.toml <- PGLORD_* env.

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::HeaderValue;
use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Listen address for the HTTP server.
    #[serde(default = "default_http_addr")]
    pub http_addr: SocketAddr,
    /// tracing-subscriber EnvFilter directive, e.g. "info" or "pglord_server=debug,info".
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Graceful drain budget; below Kubernetes' default 30s termination grace period.
    #[serde(default = "default_shutdown_timeout_secs")]
    pub shutdown_timeout_secs: u64,
    /// Comma-separated exact origins allowed by CORS. Empty = no CORS layer.
    #[serde(default = "default_empty")]
    pub cors_allowed_origins: String,
}

fn default_http_addr() -> SocketAddr { "0.0.0.0:8080".parse().expect("valid literal") }
fn default_log_level() -> String { "info".into() }
fn default_shutdown_timeout_secs() -> u64 { 25 }
fn default_empty() -> String { String::new() }

impl Config {
    pub fn load() -> anyhow::Result<Config> {
        Ok(Figment::new()
            .merge(Toml::file("config.toml"))
            .merge(Env::prefixed("PGLORD_"))
            .extract()?)
    }

    pub fn shutdown_timeout(&self) -> Duration { Duration::from_secs(self.shutdown_timeout_secs) }

    pub fn cors_origins(&self) -> Vec<HeaderValue> {
        self.cors_allowed_origins
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    /// Full literal config for tests; new fields must be added here so existing
    /// tests never break (`Config { x: y, ..Config::test_default() }`).
    pub fn test_default() -> Config {
        Config {
            http_addr: "127.0.0.1:0".parse().expect("valid literal"),
            log_level: "info".into(),
            shutdown_timeout_secs: 1,
            cors_allowed_origins: String::new(),
        }
    }
}
```

- [ ] **Step 5: Run tests** — `cargo test -p pglord-server config` → 2 passed. `make`-less lint: `cargo fmt --all && cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 6: Commit** — `git add -A && git commit -m "feat(server): figment-layered config with PGLORD_ env prefix"`

---

### Task 3: Error module (RFC 9457 problem+json)

**Files:**
- Create: `crates/pglord-server/src/error.rs`

**Interfaces:**
- Produces: `ApiError` with constructors `not_found()`, `bad_request(code)`, `conflict(code)`, `internal(source)`, builder `with_detail(...)`; `impl IntoResponse` rendering `ProblemDetails` (derives `utoipa::ToSchema`) as `application/problem+json`. Later tasks reference `crate::error::{ApiError, ProblemDetails}`.

- [ ] **Step 1: Add deps** — `serde_json = "1"`, `tracing = "0.1"`, `utoipa = { version = "5", features = ["axum_extras", "yaml"] }`. Dev-deps: `tower = { version = "0.5", features = ["util"] }`, `http-body-util = "0.1"`, `tokio = { version = "1", features = ["macros", "rt-multi-thread"] }`.

- [ ] **Step 2: Write failing test**

```rust
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
}
```

- [ ] **Step 3: Verify failure** — `cargo test -p pglord-server error` → compile error.

- [ ] **Step 4: Implement**

```rust
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
        ApiError { status, code: code.into(), detail: Cow::Borrowed(""), source: None }
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found").with_detail("The requested resource does not exist.")
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
            title: self.status.canonical_reason().unwrap_or("Error").to_string(),
            status: self.status.as_u16(),
            code: self.code.into_owned(),
            detail: self.detail.into_owned(),
        };
        let mut resp = (self.status, axum::Json(problem)).into_response();
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        resp
    }
}
```

- [ ] **Step 5: Run tests + lint** — `cargo test -p pglord-server error`, then fmt/clippy.
- [ ] **Step 6: Commit** — `git commit -am "feat(server): ApiError with RFC 9457 problem+json rendering"`

---

### Task 4: Readiness registry + health endpoints

**Files:**
- Create: `crates/pglord-server/src/health.rs`

**Interfaces:**
- Produces: `Readiness` (`Clone + Default`, `set(name, ProbeStatus)`, `snapshot()`), `ProbeStatus::{Ready, Unready}`, handlers `healthz()` and `readyz(State<Readiness>)`. Consumed by `app::build` (Task 5). Empty registry = ready. `/healthz` is pure liveness — it must never check dependencies.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn empty_registry_is_ready() {
        let r = Readiness::default();
        let resp = readyz(State(r)).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn one_unready_probe_flips_readyz_to_503() {
        let r = Readiness::default();
        r.set("kube", ProbeStatus::Unready).await;
        let resp = readyz(State(r.clone())).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        r.set("kube", ProbeStatus::Ready).await;
        let resp = readyz(State(r)).await.into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }
}
```

- [ ] **Step 2: Verify failure**, then implement:

```rust
//! Liveness and readiness. Modules owning external deps register a named probe
//! and flip it from their own health loop; /readyz aggregates.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeStatus {
    Ready,
    Unready,
}

#[derive(Clone, Default)]
pub struct Readiness(Arc<RwLock<BTreeMap<String, ProbeStatus>>>);

impl Readiness {
    pub async fn set(&self, name: impl Into<String>, status: ProbeStatus) {
        self.0.write().await.insert(name.into(), status);
    }

    pub async fn snapshot(&self) -> BTreeMap<String, ProbeStatus> {
        self.0.read().await.clone()
    }
}

/// Pure liveness: the process is up and serving. Never checks dependencies.
pub async fn healthz() -> StatusCode {
    StatusCode::OK
}

pub async fn readyz(State(readiness): State<Readiness>) -> impl IntoResponse {
    let probes = readiness.snapshot().await;
    let ready = probes.values().all(|s| *s == ProbeStatus::Ready);
    let status = if ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(serde_json::json!({
        "status": if ready { "ready" } else { "unready" },
        "probes": probes,
    })))
}
```

Requires `tokio` in main deps (`sync` feature at minimum): move `tokio = { version = "1", features = ["full"] }` into `[dependencies]`.

- [ ] **Step 3: Run tests + lint**, **Step 4: Commit** — `git commit -am "feat(server): readiness registry with healthz/readyz probes"`

---

### Task 5: app::build composition seam + first API endpoints + OpenAPI doc

**Files:**
- Create: `crates/pglord-server/src/app.rs`, `crates/pglord-server/src/api/mod.rs`, `crates/pglord-server/src/api/ping.rs`, `crates/pglord-server/src/api/version.rs`
- Test: `crates/pglord-server/tests/http.rs`

**Interfaces:**
- Consumes: `Config` (Task 2), `ApiError`/`ProblemDetails` (Task 3), `Readiness`/health handlers (Task 4).
- Produces: `app::build(readiness: Readiness, cfg: &Config) -> (axum::Router, utoipa::openapi::OpenApi)` — the single composition seam used by `main`, `--dump-openapi`, and tests. `AppState { readiness, cfg: Arc<Config> }` with `FromRef` impls.

- [ ] **Step 1: Add deps** — `utoipa-axum = "0.2"`, `utoipa-swagger-ui = { version = "9", features = ["axum"] }`, `tower-http = { version = "0.6", features = ["cors", "trace"] }`.

- [ ] **Step 2: Write failing integration test** `tests/http.rs`

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn test_app() -> axum::Router {
    let cfg = pglord_server::config::Config::test_default();
    let (router, _api) = pglord_server::app::build(pglord_server::health::Readiness::default(), &cfg);
    router
}

#[tokio::test]
async fn ping_returns_pong() {
    let resp = test_app()
        .oneshot(Request::get("/api/v1/ping").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["message"], "pong");
}

#[tokio::test]
async fn version_reports_workspace_version() {
    let resp = test_app()
        .oneshot(Request::get("/api/v1/version").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["version"], pglord_core::VERSION);
}

#[tokio::test]
async fn unknown_api_path_is_problem_json_404() {
    let resp = test_app()
        .oneshot(Request::get("/api/v1/nope").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(resp.headers()["content-type"], "application/problem+json");
}

#[tokio::test]
async fn healthz_and_readyz_live_outside_api_contract() {
    let resp = test_app()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let (_router, api) = pglord_server::app::build(
        pglord_server::health::Readiness::default(),
        &pglord_server::config::Config::test_default(),
    );
    assert!(api.paths.paths.contains_key("/api/v1/ping"));
    assert!(!api.paths.paths.contains_key("/healthz"));
}
```

This requires the server crate to expose a library: create `crates/pglord-server/src/lib.rs` with `pub mod api; pub mod app; pub mod config; pub mod error; pub mod health;` and slim `main.rs` down to call into it.

- [ ] **Step 3: Verify failure** — `cargo test -p pglord-server --test http` → compile error.

- [ ] **Step 4: Implement**

`src/api/ping.rs`:

```rust
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct PingResponse {
    pub message: String,
}

/// Unauthenticated liveness of the API surface (distinct from /healthz: this
/// one is part of the versioned contract and appears in the OpenAPI spec).
#[utoipa::path(
    get,
    path = "/api/v1/ping",
    tag = "system",
    responses((status = 200, description = "Service reachable", body = PingResponse))
)]
pub async fn ping() -> Json<PingResponse> {
    Json(PingResponse { message: "pong".into() })
}
```

`src/api/version.rs`:

```rust
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct VersionResponse {
    pub version: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/version",
    tag = "system",
    responses((status = 200, description = "Server version", body = VersionResponse))
)]
pub async fn version() -> Json<VersionResponse> {
    Json(VersionResponse { version: pglord_core::VERSION.into() })
}
```

`src/api/mod.rs`: `pub mod ping; pub mod version;`

`src/app.rs`:

```rust
//! The single composition seam: builds the full router and the OpenAPI doc.
//! Called from three places — main(), --dump-openapi, and integration tests —
//! so wiring can never drift between runtime, spec, and tests.

use std::sync::Arc;

use axum::extract::FromRef;
use axum::routing::get;
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

use crate::config::Config;
use crate::health::{self, Readiness};

#[derive(OpenApi)]
#[openapi(info(
    title = "pglord API",
    description = "Control plane for HA PostgreSQL clusters on Kubernetes."
))]
struct ApiDoc;

#[derive(Clone)]
pub struct AppState {
    pub readiness: Readiness,
    pub cfg: Arc<Config>,
}

impl FromRef<AppState> for Readiness {
    fn from_ref(s: &AppState) -> Readiness {
        s.readiness.clone()
    }
}

pub fn build(readiness: Readiness, cfg: &Config) -> (Router, utoipa::openapi::OpenApi) {
    let api = OpenApiRouter::new()
        .routes(routes!(crate::api::ping::ping))
        .routes(routes!(crate::api::version::version));

    let (api_router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(api)
        .split_for_parts();

    // Unmatched /api paths must return the same problem+json shape as handler errors.
    let api_router = api_router.fallback(|| async { crate::error::ApiError::not_found() });

    let api_router = if cfg.cors_origins().is_empty() {
        api_router
    } else {
        api_router.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(cfg.cors_origins()))
                .allow_methods(tower_http::cors::Any)
                .allow_headers([axum::http::header::CONTENT_TYPE, axum::http::header::AUTHORIZATION]),
        )
    };

    // /healthz, /readyz, /swagger-ui are merged AFTER the API router: excluded
    // from the OpenAPI contract and from CORS by construction.
    let router = api_router
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api.clone()));

    let state = AppState { readiness, cfg: Arc::new(cfg.clone()) };
    (router.with_state(state), api)
}
```

`src/lib.rs`:

```rust
//! pglord-server library surface: exposed so integration tests and the binary
//! share the exact same composition seam.

pub mod api;
pub mod app;
pub mod config;
pub mod error;
pub mod health;
```

`src/main.rs` interim: keep placeholder `main` but add `use pglord_server as _;` — fully replaced in Task 6.

- [ ] **Step 5: Run tests** — `cargo test -p pglord-server` → all pass (config, error, health units + 4 http integration tests). Lint.
- [ ] **Step 6: Commit** — `git commit -am "feat(server): app::build composition seam, ping/version API, swagger-ui"`

---

### Task 6: main.rs — tracing, --dump-openapi, graceful shutdown

**Files:**
- Modify: `crates/pglord-server/src/main.rs` (full rewrite)
- Create: `spec/openapi.yaml` (generated)

**Interfaces:**
- Consumes: `app::build`, `Config::load`, `Readiness`.
- Produces: `pglord-server` binary: `--dump-openapi` writes `spec/openapi.yaml` and exits; otherwise serves with SIGTERM/ctrl-c graceful shutdown bounded by `cfg.shutdown_timeout()`.

- [ ] **Step 1: Add deps** — `tracing-subscriber = { version = "0.3", features = ["env-filter"] }`.

- [ ] **Step 2: Implement `main.rs`**

```rust
//! Process lifecycle only. All routing/wiring lives in app::build.

use pglord_server::app;
use pglord_server::config::Config;
use pglord_server::health::Readiness;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;

    // Early exit: emit the OpenAPI artifact without touching the network.
    if std::env::args().any(|a| a == "--dump-openapi") {
        let (_router, api) = app::build(Readiness::default(), &cfg);
        std::fs::create_dir_all("spec")?;
        std::fs::write("spec/openapi.yaml", api.to_yaml()?)?;
        println!("wrote spec/openapi.yaml");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&cfg.log_level)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let readiness = Readiness::default();
    let (router, _api) = app::build(readiness, &cfg);
    let router = router.layer(
        tower_http::trace::TraceLayer::new_for_http()
            .make_span_with(tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::INFO)),
    );

    let listener = tokio::net::TcpListener::bind(cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, version = pglord_core::VERSION, "pglord-server listening");

    let server = axum::serve(listener, router).with_graceful_shutdown(shutdown_signal());
    match tokio::time::timeout(cfg.shutdown_timeout(), server.into_future()).await {
        Ok(result) => {
            result?;
            tracing::info!("drained cleanly");
        }
        Err(_) => tracing::warn!("drain exceeded shutdown timeout, forcing exit"),
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
```

Note: the timeout wraps the whole serve future — axum's graceful shutdown only begins after the signal, and `shutdown_signal()` resolves then, so the timeout must be applied to the drain, not the serve. **Correction during implementation:** wrap only the post-signal drain (spawn the server, `Notify` on signal, then `timeout(cfg.shutdown_timeout(), handle)` — signal's exact pattern). Implement the spawned-server + Notify pattern from the signal report, not the naive timeout above.

- [ ] **Step 3: Manual verification**

```bash
cargo run -p pglord-server -- --dump-openapi   # writes spec/openapi.yaml
cargo run -p pglord-server &                    # then:
curl -s localhost:8080/healthz -o /dev/null -w '%{http_code}\n'   # 200
curl -s localhost:8080/api/v1/ping                                 # {"message":"pong"}
curl -s localhost:8080/api/v1/nope | head -c 200                   # problem+json 404
kill -TERM %1                                                      # logs "drained cleanly"
```

- [ ] **Step 4: Commit** — `git add -A && git commit -m "feat(server): process lifecycle with graceful shutdown and --dump-openapi"` (includes generated `spec/openapi.yaml`).

---

### Task 7: Makefile

**Files:**
- Create: `Makefile`

**Interfaces:**
- Produces: self-documenting targets used by CI (Task 9) and the stand (Task 10): `build run release test fmt lint openapi openapi-check image stand-up stand-image stand-deploy stand-smoke stand-down clean help`.

- [ ] **Step 1: Write Makefile**

```make
IMAGE := pglord-server:dev
PROFILE := pglord
KUBECTL := minikube -p $(PROFILE) kubectl --

.DEFAULT_GOAL := help

help: ## Show this help
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build: ## Build workspace (debug)
	cargo build --workspace

run: ## Run server locally
	cargo run -p pglord-server

release: ## Build workspace (release)
	cargo build --workspace --release

test: ## Run all tests
	cargo test --workspace

fmt: ## Format code
	cargo fmt --all

lint: ## fmt --check + clippy -D warnings
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings

openapi: ## Regenerate spec/openapi.yaml
	cargo run -p pglord-server -- --dump-openapi

openapi-check: openapi ## Fail if committed spec drifts from code
	git diff --exit-code spec/

image: ## Build server docker image
	docker build -f deploy/Dockerfile -t $(IMAGE) .

stand-up: ## Start minikube dev stand (profile: pglord)
	minikube start -p $(PROFILE) --driver=docker

stand-image: image ## Build + load server image into minikube
	minikube -p $(PROFILE) image load $(IMAGE)

stand-deploy: stand-image ## Deploy pglord into the stand
	$(KUBECTL) apply -f deploy/k8s/namespace.yaml
	$(KUBECTL) apply -f deploy/k8s/

stand-smoke: ## Port-forward and curl healthz/readyz
	./scripts/stand-smoke.sh

stand-down: ## Delete the minikube stand
	minikube delete -p $(PROFILE)

clean: ## Remove build artifacts
	cargo clean
```

- [ ] **Step 2: Verify** — `make help`, `make lint`, `make test`, `make openapi-check` all succeed.
- [ ] **Step 3: Commit** — `git add Makefile && git commit -m "chore: self-documenting Makefile"`

---

### Task 8: Docker image

**Files:**
- Create: `deploy/Dockerfile`, `.dockerignore`

- [ ] **Step 1: `.dockerignore`**

```
target
.git
docs
spec
deploy/k8s
**/*.md
```

- [ ] **Step 2: `deploy/Dockerfile`** (cargo-chef, distroless, non-root — signal's proven recipe)

```dockerfile
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release -p pglord-server

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
WORKDIR /app
COPY --from=builder /app/target/release/pglord-server /usr/local/bin/pglord-server
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/pglord-server"]
```

- [ ] **Step 3: Verify** — `make image`, then `docker run --rm -p 18080:8080 pglord-server:dev &` and `curl -s localhost:18080/healthz -w '%{http_code}'` → 200. Stop container.
- [ ] **Step 4: Commit** — `git add -A && git commit -m "chore: cargo-chef distroless server image"`

---

### Task 9: CI workflow (with OpenAPI drift gate)

**Files:**
- Create: `.github/workflows/ci.yaml`

- [ ] **Step 1: Write workflow**

```yaml
name: ci
on:
  push:
    branches: [main, master]
  pull_request:

jobs:
  build-test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --workspace
      - name: OpenAPI drift gate
        run: |
          cargo run -p pglord-server -- --dump-openapi
          git diff --exit-code spec/
```

- [ ] **Step 2: Verify locally** — `make lint && make test && make openapi-check`.
- [ ] **Step 3: Commit** — `git add .github && git commit -m "ci: build, lint, test, openapi drift gate"`

---

### Task 10: Minikube dev stand

**Files:**
- Create: `deploy/k8s/namespace.yaml`, `deploy/k8s/rbac.yaml`, `deploy/k8s/deployment.yaml`, `deploy/k8s/service.yaml`, `scripts/stand-smoke.sh`

**Interfaces:**
- Consumes: Makefile targets (Task 7), image (Task 8).
- Produces: running pglord-server in namespace `pglord` on minikube profile `pglord`; `make stand-smoke` green.

- [ ] **Step 1: Install tooling if missing** (host-level, idempotent)

```bash
command -v minikube || {
  curl -fsSL https://storage.googleapis.com/minikube/releases/latest/minikube-linux-amd64 \
    -o ~/.local/bin/minikube && chmod +x ~/.local/bin/minikube
}
docker info >/dev/null   # docker must work in WSL2; abort with clear message if not
```

`kubectl` comes via `minikube kubectl --` (aliased as `KUBECTL` in the Makefile) — no separate install.

- [ ] **Step 2: Manifests**

`deploy/k8s/namespace.yaml`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: pglord
```

`deploy/k8s/rbac.yaml` (namespaced Role sized for the coming reconciler — statefulsets/services/configmaps/secrets/pods):

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: pglord-server
  namespace: pglord
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: pglord-server
  namespace: pglord
rules:
  - apiGroups: [""]
    resources: [configmaps, secrets, services, pods]
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups: [apps]
    resources: [statefulsets]
    verbs: [get, list, watch, create, update, patch, delete]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: pglord-server
  namespace: pglord
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: pglord-server
subjects:
  - kind: ServiceAccount
    name: pglord-server
    namespace: pglord
```

`deploy/k8s/deployment.yaml`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pglord-server
  namespace: pglord
spec:
  replicas: 1
  selector:
    matchLabels: { app: pglord-server }
  template:
    metadata:
      labels: { app: pglord-server }
    spec:
      serviceAccountName: pglord-server
      containers:
        - name: server
          image: pglord-server:dev
          imagePullPolicy: Never   # loaded via `minikube image load`
          ports: [{ containerPort: 8080, name: http }]
          env:
            - { name: PGLORD_LOG_LEVEL, value: "info" }
          livenessProbe:
            httpGet: { path: /healthz, port: http }
          readinessProbe:
            httpGet: { path: /readyz, port: http }
          resources:
            requests: { cpu: 50m, memory: 32Mi }
            limits: { memory: 128Mi }
```

`deploy/k8s/service.yaml`:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: pglord-server
  namespace: pglord
spec:
  selector: { app: pglord-server }
  ports: [{ name: http, port: 80, targetPort: http }]
```

`scripts/stand-smoke.sh` (chmod +x):

```bash
#!/usr/bin/env bash
# Smoke-test the minikube stand: healthz + readyz + ping through a port-forward.
set -euo pipefail
KUBECTL="minikube -p pglord kubectl --"
$KUBECTL -n pglord rollout status deploy/pglord-server --timeout=120s
$KUBECTL -n pglord port-forward svc/pglord-server 18080:80 &
PF=$!
trap 'kill $PF 2>/dev/null || true' EXIT
sleep 2
for path in healthz readyz api/v1/ping; do
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:18080/$path")
  [ "$code" = "200" ] || { echo "FAIL /$path -> $code"; exit 1; }
  echo "OK /$path"
done
echo "stand smoke passed"
```

- [ ] **Step 3: Bring up + verify**

```bash
make stand-up        # minikube start -p pglord --driver=docker
make stand-deploy    # image build + load + kubectl apply
make stand-smoke     # OK /healthz, OK /readyz, OK /api/v1/ping
```

- [ ] **Step 4: Commit** — `git add -A && git commit -m "feat: minikube dev stand with k8s manifests and smoke test"`

---

## Verification (end of plan)

1. `make lint && make test` — clean.
2. `make openapi-check` — spec artifact matches code.
3. `make image` — image builds; container serves `/healthz`.
4. `make stand-up stand-deploy stand-smoke` — pglord-server runs on minikube, probes green, ping 200.
5. `git log --oneline` — one conventional commit per task.
