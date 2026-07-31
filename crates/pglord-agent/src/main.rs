//! pglord-agent entrypoint: bootstrap PGDATA, run postgres as a supervised
//! child, and serve the internal control endpoint until either exits.

use std::sync::Arc;

use pglord_agent::config::Config;
use pglord_agent::{bootstrap, http, supervisor};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(Config::load()?);
    tracing::info!(role = ?cfg.role, version = env!("CARGO_PKG_VERSION"), "pglord-agent starting");

    bootstrap::ensure_bootstrapped(&cfg).await?;
    let child = supervisor::spawn_postgres(&cfg)?;

    let listener = tokio::net::TcpListener::bind(cfg.http_addr).await?;
    tracing::info!(addr = %cfg.http_addr, "control endpoint listening");
    let http_server = axum::serve(listener, http::router(cfg.clone()));

    tokio::select! {
        code = supervisor::supervise(child) => {
            let code = code?;
            tracing::info!(code, "postgres supervision finished, exiting");
            std::process::exit(code);
        }
        result = http_server.into_future() => {
            result?;
            anyhow::bail!("control endpoint stopped unexpectedly");
        }
    }
}
